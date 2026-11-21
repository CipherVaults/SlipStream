use super::*;
use md5::Md5;
use sha2::{Digest, Sha256};
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

const KIB: usize = 1024;
const CHUNK: usize = 16 * KIB;
const WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn range(&self) -> Option<Range<usize>> {
        let (start, last) = self
            .header("range")?
            .strip_prefix("bytes=")?
            .split_once('-')?;
        Some(start.parse().unwrap()..last.parse::<usize>().unwrap() + 1)
    }
}

struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    content: Option<(Arc<Vec<u8>>, Range<usize>)>,
    body_gate: Option<watch::Receiver<bool>>,
    initial_delay: Duration,
    chunk_delay: Duration,
    cutoff: Option<usize>,
    chunked: bool,
}

impl Reply {
    fn empty(status: &'static str) -> Self {
        Self {
            status,
            headers: vec![("Content-Length", "0".into())],
            content: None,
            body_gate: None,
            initial_delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
            cutoff: None,
            chunked: false,
        }
    }

    fn redirect(to: &str) -> Self {
        let mut reply = Self::empty("302 Found");
        reply.headers.push(("Location", to.to_string()));
        reply
    }

    fn resource(request: &Request, data: &Arc<Vec<u8>>, etag: &str) -> Self {
        let mut status = "200 OK";
        let mut headers = vec![
            ("Accept-Ranges", "bytes".into()),
            ("ETag", etag.to_string()),
        ];
        let mut bounds = 0..data.len();
        if let Some(range) = request.range().filter(|_| {
            request
                .header("if-range")
                .is_none_or(|validator| validator == etag)
        }) {
            if range.end > data.len() {
                let mut empty = Self::empty("416 Range Not Satisfiable");
                empty
                    .headers
                    .push(("Content-Range", format!("bytes */{}", data.len())));
                return empty;
            }
            status = "206 Partial Content";
            headers.push((
                "Content-Range",
                format!("bytes {}-{}/{}", range.start, range.end - 1, data.len()),
            ));
            bounds = range;
        }
        headers.push(("Content-Length", bounds.len().to_string()));
        Self {
            headers,
            content: Some((Arc::clone(data), bounds)),
            ..Self::empty(status)
        }
    }

    fn with_delay(mut self, duration: Duration) -> Self {
        self.chunk_delay = duration;
        self
    }

    async fn send(self, request: &Request, stream: &mut TcpStream) -> io::Result<()> {
        let mut header = format!("HTTP/1.1 {}\r\nConnection: close\r\n", self.status);
        for (name, value) in self.headers {
            header.push_str(&format!("{name}: {value}\r\n"));
        }
        header.push_str("\r\n");
        stream.write_all(header.as_bytes()).await?;
        if request.method == "HEAD" {
            return stream.shutdown().await;
        }
        if let Some((data, bounds)) = self.content {
            if let Some(mut gate) = self.body_gate {
                let _ = gate.wait_for(|open| *open).await;
            }
            if !self.initial_delay.is_zero() {
                sleep(self.initial_delay).await;
            }
            let end = self
                .cutoff
                .map_or(bounds.end, |cutoff| bounds.end.min(bounds.start + cutoff));
            for chunk in data[bounds.start..end].chunks(CHUNK) {
                if self.chunked {
                    stream
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .await?;
                }
                stream.write_all(chunk).await?;
                if self.chunked {
                    stream.write_all(b"\r\n").await?;
                }
                if !self.chunk_delay.is_zero() {
                    sleep(self.chunk_delay).await;
                }
            }
            if self.chunked {
                stream.write_all(b"0\r\n\r\n").await?;
            }
        }
        stream.shutdown().await
    }
}

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn new(reply: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let handler = Arc::new(reply);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        let log = Arc::clone(&log);
                        let reply = Arc::clone(&handler);
                        connections.spawn(async move {
                            if let Ok(request) = read_request(&mut stream).await {
                                log.lock().unwrap().push(request.clone());
                                let _ = reply(&request).send(&request, &mut stream).await;
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).await?;
    let mut first = first.split_whitespace();
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid test request");
    let method = first.next().ok_or_else(invalid)?.to_owned();
    let path = first.next().ok_or_else(invalid)?.to_owned();
    let mut headers = HashMap::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').ok_or_else(invalid)?;
        headers.insert(key.to_ascii_lowercase(), value.trim().into());
    }
    Ok(Request {
        method,
        path,
        headers,
    })
}

const IO_TEST_SLOTS: u32 = 32;
static IO_TESTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(IO_TEST_SLOTS as usize);

struct TestDir {
    path: PathBuf,
    _slot: tokio::sync::SemaphorePermit<'static>,
}

impl TestDir {
    async fn new() -> Self {
        Self::with_slots(1).await
    }

    async fn with_slots(slots: u32) -> Self {
        let permit = IO_TESTS.acquire_many(slots).await.unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("engine-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self {
            path,
            _slot: permit,
        }
    }

    fn config(&self) -> EngineConfig {
        EngineConfig {
            db_path: self.path.join("state.sqlite"),
            default_download_dir: self.path.join("downloads"),
            allow_loopback: true,
            request_timeout: Duration::from_secs(5),
            ..EngineConfig::default()
        }
    }

    async fn assert_output(&self, filename: &str, expected: &[u8]) {
        let path = self.config().default_download_dir.join(filename);
        let actual = tokio::fs::read(path).await.unwrap();
        assert_eq!(actual.len(), expected.len());
        assert!(actual == expected, "output differs from the served bytes");
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn payload(size: usize, mut seed: u32) -> Arc<Vec<u8>> {
    Arc::new(
        (0..size)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed >> 8) as u8
            })
            .collect(),
    )
}

fn request(url: String, filename: Option<&str>, connections: u8) -> AddHttpDownloadRequest {
    AddHttpDownloadRequest {
        url,
        filename: filename.map(str::to_owned),
        save_dir: None,
        max_connections: Some(connections),
        queue_id: None,
        headers: HashMap::new(),
        referrer: None,
        expected_sha256: None,
    }
}

async fn wait_for(
    engine: &DownloadEngine,
    id: &str,
    condition: impl Fn(&TransferSnapshot) -> bool,
) -> TransferSnapshot {
    timeout(WAIT, async {
        loop {
            let status = engine.snapshot(id).unwrap();
            if condition(&status) {
                return status;
            }
            assert_ne!(
                status.status,
                TransferStatus::Failed,
                "{:?}",
                status.error_message
            );
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out at {:?}", engine.snapshot(id).unwrap()))
}

async fn completed(engine: &DownloadEngine, id: &str) -> TransferSnapshot {
    wait_for(engine, id, |s| s.status == TransferStatus::Completed).await
}

async fn failed(engine: &DownloadEngine, id: &str) -> TransferSnapshot {
    wait_for(engine, id, |s| s.status == TransferStatus::Failed).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_workers_dynamically_split_a_live_range() {
    const WIDTH: usize = 768 * KIB;
    let data = payload(16 * WIDTH, 1);
    let source = Arc::clone(&data);
    let (release, hold) = watch::channel(false);
    let server = Server::new(move |req| {
        let mut reply = Reply::resource(req, &source, "\"v1\"");
        if req
            .range()
            .is_some_and(|r| r.start != 0 && r.start % WIDTH == 0)
        {
            reply.body_gate = Some(hold.clone());
            reply.chunk_delay = Duration::from_millis(2);
        } else if req.range().is_some_and(|r| r.start == 0) {
            reply.initial_delay = Duration::from_millis(80);
        }
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let mut events = engine.subscribe_events();
    let mut telemetry = engine.subscribe_telemetry();
    let id = engine
        .add_http_download(request(server.url("/file.bin"), None, 16))
        .await
        .unwrap();
    let batch = timeout(WAIT, async {
        loop {
            let batch = telemetry.recv().await.unwrap();
            if batch.active_threads == 16 {
                break batch;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(batch.active_count, 1);
    assert_eq!(batch.transfers[0].ui_segments.len(), 16);
    let split = timeout(WAIT, async {
        loop {
            if let EngineEvent::SegmentSplit {
                transfer_id,
                split_point,
                ..
            } = events.recv().await.unwrap()
            {
                assert_eq!(transfer_id, id);
                break split_point;
            }
        }
    })
    .await
    .unwrap();
    assert!(split > WIDTH as u64);
    assert_eq!(engine.snapshot(&id).unwrap().status, TransferStatus::Active);
    release.send_replace(true);
    let completed = completed(&engine, &id).await;
    assert_eq!(completed.protocol_label, "HTTP/1.1");
    assert_eq!(completed.ui_segments, [1.0; 16]);
    assert!(completed.segments.len() > 16);
    assert!(server
        .requests()
        .iter()
        .any(|req| req.range().is_some_and(|r| r.start as u64 == split)));
    dir.assert_output("file.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn retries_socket_drops_from_the_persisted_cursor() {
    let data = payload(512 * KIB, 2);
    let source = Arc::clone(&data);
    let requests = AtomicUsize::new(0);
    let server = Server::new(move |req| {
        let mut reply = Reply::resource(req, &source, "\"retry\"");
        if req.method == "GET" && requests.fetch_add(1, Ordering::SeqCst) < 3 {
            reply.cutoff = Some(64 * KIB);
        }
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/retry.bin"), None, 1))
        .await
        .unwrap();
    completed(&engine, &id).await;
    let starts: Vec<_> = server
        .requests()
        .iter()
        .filter_map(Request::range)
        .map(|r| r.start)
        .collect();
    assert_eq!(starts, [0, 64 * KIB, 128 * KIB, 192 * KIB]);
    dir.assert_output("retry.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn paused_cursors_survive_database_close_and_reopen() {
    let data = payload(2048 * KIB, 3);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        Reply::resource(req, &source, "\"persist\"").with_delay(Duration::from_millis(7))
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/persistent.bin"), None, 4))
        .await
        .unwrap();
    wait_for(&engine, &id, |s| s.downloaded_bytes >= 64 * KIB as u64).await;
    engine.pause_transfer(&id).await.unwrap();
    let before = engine.snapshot(&id).unwrap();
    assert_eq!(before.status, TransferStatus::Paused);
    assert!((1..data.len() as u64).contains(&before.downloaded_bytes));
    let config = dir.config();
    engine.shutdown().await.unwrap();
    drop(engine);
    let engine = DownloadEngine::new(config).await.unwrap();
    let after = engine.snapshot(&id).unwrap();
    assert_eq!(after.status, TransferStatus::Paused);
    assert!(after.resumable);
    assert_eq!(after.downloaded_bytes, before.downloaded_bytes);
    assert_eq!(after.ui_segments, before.ui_segments);
    let cursors: Vec<_> = before
        .segments
        .iter()
        .filter_map(|segment| {
            let cursor = segment.start_byte + segment.downloaded_bytes;
            (cursor < segment.end_byte).then_some(cursor)
        })
        .collect();
    let earlier = server.requests().len();
    engine.resume_transfer(&id).await.unwrap();
    completed(&engine, &id).await;
    let resumed = server.requests();
    assert!(cursors
        .iter()
        .all(|cursor| resumed[earlier..].iter().any(|req| {
            req.header("if-range") == Some("\"persist\"")
                && req.range().is_some_and(|r| r.start as u64 == *cursor)
        })));
    dir.assert_output("persistent.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_pauses_abandoned_runs_and_reconciles_an_unrecorded_rename() {
    let data = payload(512 * KIB, 13);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        Reply::resource(req, &source, "\"crash\"").with_delay(Duration::from_millis(7))
    })
    .await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let partial = engine
        .add_http_download(request(server.url("/crashed.bin"), None, 1))
        .await
        .unwrap();
    wait_for(&engine, &partial, |s| s.downloaded_bytes >= 64 * KIB as u64).await;
    engine.pause_transfer(&partial).await.unwrap();
    let cursor = engine.snapshot(&partial).unwrap().downloaded_bytes;
    let renamed = engine
        .add_http_download(request(server.url("/committed.bin"), None, 1))
        .await
        .unwrap();
    completed(&engine, &renamed).await;
    engine.shutdown().await.unwrap();
    let probe_id = Uuid::new_v4().to_string();
    {
        let mut db = Database::open(&config.db_path).unwrap();
        for (mut record, mut segments) in db.transfers().unwrap() {
            record.set_status(TransferStatus::Active);
            if record.id == renamed {
                record.commit_digest = Some(hex::encode(Sha256::digest(&*data)));
            } else {
                for segment in segments.values_mut() {
                    segment.state = SegmentState::Active;
                }
                let mut probing = record.clone();
                probing.id = probe_id.clone();
                probing.set_status(TransferStatus::Probing);
                probing.ranges = false;
                probing.total = None;
                db.save_record(&probing).unwrap();
            }
            db.replace_segments(&record, &segments).unwrap();
        }
    }
    let engine = DownloadEngine::new(config).await.unwrap();
    let recovered = engine.snapshot(&partial).unwrap();
    assert_eq!(recovered.status, TransferStatus::Paused);
    assert!(recovered.resumable);
    assert_eq!(recovered.downloaded_bytes, cursor);
    let probing = engine.snapshot(&probe_id).unwrap();
    assert_eq!(probing.status, TransferStatus::Paused);
    assert!(!probing.resumable);
    assert_eq!(
        engine.snapshot(&renamed).unwrap().status,
        TransferStatus::Completed
    );
    engine.resume_transfer(&partial).await.unwrap();
    completed(&engine, &partial).await;
    dir.assert_output("crashed.bin", &data).await;
    dir.assert_output("committed.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_if_range_restarts_all_workers_with_new_bytes_and_size() {
    let old = payload(1024 * KIB, 4);
    let new = payload(1408 * KIB, 5);
    let new_source = Arc::clone(&new);
    let updated = Arc::new(AtomicBool::new(false));
    let switch = Arc::clone(&updated);
    let server = Server::new(move |req| {
        if switch.load(Ordering::SeqCst) {
            Reply::resource(req, &new_source, "\"v2\"")
        } else {
            Reply::resource(req, &old, "\"v1\"").with_delay(Duration::from_millis(10))
        }
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/changed.bin"), None, 4))
        .await
        .unwrap();
    wait_for(&engine, &id, |s| s.downloaded_bytes >= 64 * KIB as u64).await;
    engine.pause_transfer(&id).await.unwrap();
    updated.store(true, Ordering::SeqCst);
    engine.resume_transfer(&id).await.unwrap();
    let done = completed(&engine, &id).await;
    assert_eq!(done.total_bytes, Some(new.len() as u64));
    let log = server.requests();
    let stale = log
        .iter()
        .position(|req| req.header("if-range") == Some("\"v1\""))
        .unwrap();
    assert!(log[stale + 1..].iter().any(|req| req.range() == Some(0..1)));
    assert!(log[stale + 1..]
        .iter()
        .any(|req| req.range().is_some_and(|r| r.start == 0 && r.len() > 1)));
    dir.assert_output("changed.bin", &new).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_transfer_scrubs_persisted_request_credentials() {
    let data = payload(64 * KIB, 6);
    let (_release, hold) = watch::channel(false);
    let server = Server::new(move |req| {
        let mut reply = Reply::resource(req, &data, "\"cancel\"");
        if req.method == "GET" && req.range() != Some(0..1) {
            reply.body_gate = Some(hold.clone());
        }
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let signed_url = server.url("/signed.bin?token=private#fragment");
    let mut download = request(signed_url.clone(), None, 1);
    download.headers = [("Authorization".into(), "Bearer private".into())].into();
    download.referrer = Some("https://example.com/source?token=private".into());
    let id = engine.add_http_download(download).await.unwrap();
    let active = wait_for(&engine, &id, |snapshot| {
        snapshot.status == TransferStatus::Active
    })
    .await;
    assert_eq!(active.source_url, signed_url);

    engine.cancel_transfer(&id).await.unwrap();
    engine.shutdown().await.unwrap();

    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let canceled = engine.snapshot(&id).unwrap();
    assert_eq!(canceled.status, TransferStatus::Canceled);
    assert_eq!(canceled.source_url, server.url("/signed.bin"));
    let records = Database::open(&config.db_path)
        .unwrap()
        .transfers()
        .unwrap();
    let (record, _) = records.iter().find(|(record, _)| record.id == id).unwrap();
    assert!(record.headers.is_empty());
    assert_eq!(record.referrer, None);
    assert_eq!(record.source_url, canceled.source_url);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn refresh_failed_url_preserves_bytes_and_sends_new_credentials() {
    let data = payload(512 * KIB, 6);
    let source = Arc::clone(&data);
    let served = AtomicBool::new(false);
    let server = Server::new(move |req| {
        let mut reply = Reply::resource(req, &source, "\"refresh\"");
        if req.path == "/expired" && req.method == "GET" {
            if served.swap(true, Ordering::SeqCst) {
                return Reply::empty("410 Gone");
            }
            reply.cutoff = Some(128 * KIB);
        }
        if req.path == "/fresh" && req.header("authorization") != Some("Bearer updated") {
            return Reply::empty("403 Forbidden");
        }
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/expired"), Some("refresh.bin"), 1))
        .await
        .unwrap();
    let before = failed(&engine, &id).await;
    assert_eq!(before.downloaded_bytes, 128 * KIB as u64);
    engine
        .refresh_download_address(RefreshDownloadUrlRequest {
            transfer_id: id.clone(),
            new_url: server.url("/fresh"),
            headers: [("Authorization".into(), "Bearer updated".into())].into(),
            referrer: Some("https://example.com/source".into()),
        })
        .await
        .unwrap();
    let refreshed = engine.snapshot(&id).unwrap();
    assert_eq!(refreshed.status, TransferStatus::Paused);
    assert_eq!(refreshed.downloaded_bytes, before.downloaded_bytes);
    assert_eq!(refreshed.segments[0].end_byte, before.segments[0].end_byte);
    engine.shutdown().await.unwrap();
    let engine = DownloadEngine::new(config).await.unwrap();
    engine.resume_transfer(&id).await.unwrap();
    completed(&engine, &id).await;
    for req in server.requests().iter().filter(|req| req.path == "/fresh") {
        assert_eq!(req.range().unwrap().start, 128 * KIB);
        assert_eq!(req.header("authorization"), Some("Bearer updated"));
        assert_eq!(req.header("if-range"), Some("\"refresh\""));
        assert_eq!(req.header("referer"), Some("https://example.com/source"));
    }
    assert!(server.requests().iter().any(|req| req.path == "/fresh"));
    dir.assert_output("refresh.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn cross_origin_redirect_strips_credentials_permanently() {
    let data = payload(8 * KIB, 7);
    let source = Arc::clone(&data);
    let first_origin = Arc::new(Mutex::new(String::new()));
    let return_to = Arc::clone(&first_origin);
    let target = Server::new(move |_| Reply::redirect(&return_to.lock().unwrap())).await;
    let destination = target.url("/other");
    let origin = Server::new(move |req| {
        if req.path == "/initial" {
            Reply::redirect(&destination)
        } else {
            Reply::resource(req, &source, "\"cross\"")
        }
    })
    .await;
    *first_origin.lock().unwrap() = origin.url("/final");
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let mut download = request(origin.url("/initial"), Some("redirect.bin"), 1);
    let sensitive = ["cookie", "cookie2", "authorization", "proxy-authorization"];
    download.headers = sensitive
        .into_iter()
        .map(|key| (key.into(), "secret".into()))
        .collect();
    let id = engine.add_http_download(download).await.unwrap();
    completed(&engine, &id).await;
    for req in origin.requests() {
        for header in sensitive {
            assert_eq!(
                req.header(header),
                (req.path == "/initial").then_some("secret")
            );
        }
    }
    assert!(!target.requests().is_empty());
    for req in target.requests() {
        for header in sensitive {
            assert_eq!(req.header(header), None);
        }
    }
    dir.assert_output("redirect.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn redirects_to_private_and_link_local_are_blocked_even_in_loopback_mode() {
    let server = Server::new(|req| {
        Reply::redirect(if req.path == "/link-local" {
            "http://169.254.169.254/metadata"
        } else {
            "http://10.0.0.1/private"
        })
    })
    .await;
    let dir = TestDir::new().await;
    let mut config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    for (path, blocked) in [("/link-local", "169.254.169.254"), ("/private", "10.0.0.1")] {
        let id = engine
            .add_http_download(request(server.url(path), None, 1))
            .await
            .unwrap();
        assert!(failed(&engine, &id)
            .await
            .error_message
            .unwrap()
            .contains(blocked));
    }
    engine.shutdown().await.unwrap();

    config.allow_loopback = false;
    let engine = DownloadEngine::new(config).await.unwrap();
    assert!(matches!(
        engine.add_http_download(request(server.url("/private"), None, 1)).await,
        Err(EngineError::BlockedAddress(ip)) if ip.is_loopback()
    ));
    let id = engine
        .add_http_download(request(
            server.url("/private").replace("127.0.0.1", "localhost"),
            None,
            1,
        ))
        .await
        .unwrap();
    assert!(failed(&engine, &id)
        .await
        .error_message
        .unwrap()
        .contains("blocked connection"));
    engine.shutdown().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_ads_moves_with_the_download_and_sanitizes_newlines() {
    let data = payload(8 * KIB, 8);
    let server = Server::new(move |req| Reply::resource(req, &data, "\"ads\"")).await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let url = server.url("/mark.bin");
    let mut download = request(url.clone(), None, 1);
    let referrer = "https://example.com/downloads";
    download.referrer = Some(referrer.into());
    let id = engine.add_http_download(download).await.unwrap();
    completed(&engine, &id).await;
    let path = config.default_download_dir.join("mark.bin");
    let ads = tokio::fs::read_to_string(path.with_file_name("mark.bin:Zone.Identifier"))
        .await
        .unwrap();
    assert_eq!(
        ads,
        format!("[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl={referrer}\r\nHostUrl={url}\r\n")
    );
    output::mark_of_the_web(
        &path,
        "https://user:secret@example.com/\r\nsource",
        Some("https://alice:pw@ref.example.com/list"),
    )
    .unwrap();
    let ads = tokio::fs::read_to_string(path.with_file_name("mark.bin:Zone.Identifier"))
        .await
        .unwrap();
    assert_eq!(
        ads,
        "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://ref.example.com/list\r\nHostUrl=https://example.com/source\r\n"
    );
    engine.shutdown().await.unwrap();
}

#[test]
fn wildcard_expands_padded_unpadded_stepped_and_multiple_ranges() {
    let urls = expand_wildcard_url("https://example.com/file_[001-010].zip").unwrap();
    assert_eq!(urls.len(), 10);
    assert_eq!(urls[0], "https://example.com/file_001.zip");
    assert_eq!(urls[9], "https://example.com/file_010.zip");
    assert_eq!(
        expand_wildcard_url("https://example.com/[a-c]/[7-11:2].zip").unwrap(),
        ["a/7", "a/9", "a/11", "b/7", "b/9", "b/11", "c/7", "c/9", "c/11"]
            .map(|suffix| format!("https://example.com/{suffix}.zip"))
    );
    assert_eq!(
        expand_wildcard_url("http://[::1]/[A-C:2]").unwrap(),
        ["http://[::1]/A", "http://[::1]/C"]
    );
    for invalid in [
        "file:///tmp/[001-002].zip",
        "https://example.com/[1-3:0]",
        "https://example.com/[4-3]",
        "https://example.com/[1-101]/[1-100]",
        "https://example.com/[1-3",
    ] {
        assert!(expand_wildcard_url(invalid).is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn limits_track_bytes_per_second_and_live_updates_release_waiters() {
    let data = payload(256 * KIB, 9);
    let source = Arc::clone(&data);
    let (release, hold) = watch::channel(false);
    let server = Server::new(move |req| {
        let mut reply = Reply::resource(req, &source, "\"speed\"");
        if req.path == "/limited.bin" {
            reply.body_gate = Some(hold.clone());
        }
        reply
    })
    .await;
    // Keep parallel Windows file and SQLite flushes out of the throughput measurement.
    let dir = TestDir::with_slots(IO_TEST_SLOTS).await;
    let mut config = dir.config();
    config.global_speed_limit_bps = Some(128 * KIB as u64);
    let engine = DownloadEngine::new(config).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/limited.bin"), None, 2))
        .await
        .unwrap();
    wait_for(&engine, &id, |s| s.status == TransferStatus::Active).await;
    let start = Instant::now();
    release.send_replace(true);
    wait_for(&engine, &id, |s| s.downloaded_bytes == data.len() as u64).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1700) && elapsed < Duration::from_millis(3500),
        "{elapsed:?}"
    );
    completed(&engine, &id).await;
    engine.set_global_speed_limit(Some(1)).await.unwrap();
    let id = engine
        .add_http_download_with_options(
            request(server.url("/updated.bin"), None, 2),
            ExtendedHttpOptions {
                speed_limit_bps: Some(32 * KIB as u64),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    wait_for(&engine, &id, |s| s.status == TransferStatus::Active).await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(engine.snapshot(&id).unwrap().downloaded_bytes, 0);
    engine.set_global_speed_limit(None).await.unwrap();
    wait_for(&engine, &id, |s| s.downloaded_bytes >= CHUNK as u64).await;
    let start = Instant::now();
    engine.set_transfer_speed_limit(&id, None).await.unwrap();
    completed(&engine, &id).await;
    assert!(start.elapsed() < Duration::from_secs(3));
    dir.assert_output("updated.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn checksums_verify_and_mismatches_keep_final_path_uncommitted() {
    let data = payload(64 * KIB, 10);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| Reply::resource(req, &source, "\"digest\"")).await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let sha = hex::encode(Sha256::digest(&*data));
    let md5 = hex::encode(Md5::digest(&*data));
    for (name, expected_sha, expected_md5, verified, mismatch) in [
        ("good.bin", sha.clone(), md5.clone(), true, None),
        ("bad-sha.bin", "0".repeat(64), md5, false, Some("SHA-256")),
        ("bad-md5.bin", sha, "0".repeat(32), true, Some("MD5")),
    ] {
        let mut download = request(server.url("/hash"), Some(name), 2);
        download.expected_sha256 = Some(expected_sha);
        let id = engine
            .add_http_download_with_options(
                download,
                ExtendedHttpOptions {
                    expected_md5: Some(expected_md5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let snapshot = if let Some(mismatch) = mismatch {
            let snapshot = failed(&engine, &id).await;
            assert!(snapshot.error_message.as_ref().unwrap().contains(mismatch));
            assert!(!config.default_download_dir.join(name).exists());
            snapshot
        } else {
            let snapshot = completed(&engine, &id).await;
            dir.assert_output(name, &data).await;
            snapshot
        };
        assert_eq!(snapshot.sha256_verified, Some(verified));
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_caps_promote_waiters_and_emit_post_actions() {
    let data = payload(512 * KIB, 11);
    let server = Server::new(move |req| {
        Reply::resource(req, &data, "\"queue\"").with_delay(Duration::from_millis(12))
    })
    .await;
    let dir = TestDir::new().await;
    let mut config = dir.config();
    config.max_concurrent_downloads = 2;
    let engine = DownloadEngine::new(config).await.unwrap();
    engine
        .set_queue_config(QueueConfig {
            id: "work".into(),
            max_concurrent_downloads: 1,
            post_action: PostQueueAction::Shutdown,
        })
        .await
        .unwrap();
    let mut events = engine.subscribe_events();
    let add = |name: &'static str, queue: &'static str| {
        let mut download = request(server.url(name), None, 1);
        download.queue_id = Some(queue.into());
        engine.add_http_download(download)
    };
    let a = add("/a.bin", "work").await.unwrap();
    let b = add("/b.bin", "work").await.unwrap();
    let c = add("/c.bin", DEFAULT_QUEUE).await.unwrap();
    let d = add("/d.bin", DEFAULT_QUEUE).await.unwrap();
    wait_for(&engine, &a, |s| s.status == TransferStatus::Active).await;
    wait_for(&engine, &c, |s| s.status == TransferStatus::Active).await;
    assert_eq!(engine.snapshot(&b).unwrap().status, TransferStatus::Queued);
    assert_eq!(engine.snapshot(&d).unwrap().status, TransferStatus::Queued);
    engine.pause_transfer(&a).await.unwrap();
    wait_for(&engine, &b, |s| s.status == TransferStatus::Active).await;
    completed(&engine, &b).await;
    completed(&engine, &d).await;
    assert!(!std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        matches!(event, EngineEvent::QueueDrained { queue_id, .. } if queue_id == "work")
    }));
    engine.cancel_transfer(&a).await.unwrap();
    timeout(WAIT, async {
        loop {
            if let EngineEvent::QueueDrained { queue_id, action } = events.recv().await.unwrap() {
                if queue_id == "work" {
                    assert_eq!(action, PostQueueAction::Shutdown);
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn probes_unknown_lengths_empty_files_and_utf8_disposition() {
    let data = payload(90 * KIB, 12);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        if req.path == "/empty" {
            return Reply::empty("200 OK");
        }
        if req.method == "HEAD" {
            return Reply::empty("405 Method Not Allowed");
        }
        let mut reply = Reply::resource(req, &source, "\"stream\"");
        reply.status = "200 OK";
        reply.content = Some((Arc::clone(&source), 0..source.len()));
        reply.headers = vec![
            ("Transfer-Encoding", "chunked".into()),
            (
                "Content-Disposition",
                "attachment; filename=\"old.bin\"; filename*=UTF-8''caf%C3%A9.bin".into(),
            ),
        ];
        reply.chunked = true;
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let stream = engine
        .add_http_download(request(server.url("/stream"), None, 32))
        .await
        .unwrap();
    let done = completed(&engine, &stream).await;
    assert_eq!(done.total_bytes, Some(data.len() as u64));
    assert_eq!(done.segments.len(), 1);
    assert!(!done.resumable);
    assert_eq!(done.filename, "café.bin");
    dir.assert_output("café.bin", &data).await;
    let empty = engine
        .add_http_download(request(server.url("/empty"), None, 16))
        .await
        .unwrap();
    let done = completed(&engine, &empty).await;
    assert_eq!(done.total_bytes, Some(0));
    assert_eq!(done.ui_segments, [1.0; 16]);
    dir.assert_output("empty", &[]).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn adversarial_content_disposition_never_becomes_a_file_path() {
    let server = Server::new(|req| {
        let name = match req.path.as_str() {
            "/traversal" => "../evil",
            "/device" => "CON.exe",
            "/ads" => "evil:Zone.Identifier",
            _ => "\u{202e}evil",
        };
        let mut reply = Reply::empty("200 OK");
        reply.headers.push((
            "Content-Disposition",
            format!("attachment; filename=\"{name}\""),
        ));
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    for path in ["/traversal", "/device", "/ads", "/bidi"] {
        let id = engine
            .add_http_download(request(server.url(path), None, 1))
            .await
            .unwrap();
        assert!(failed(&engine, &id).await.error_message.is_some());
    }
    assert_eq!(
        std::fs::read_dir(config.default_download_dir)
            .unwrap()
            .count(),
        0
    );
    engine.shutdown().await.unwrap();
}

#[test]
fn telemetry_projection_uses_byte_overlap_for_small_files_and_uneven_ranges() {
    use storage::Segment;
    let mut first = Segment::new(0, 0, Some(2), SegmentState::Completed);
    first.cursor = 2;
    let mut second = Segment::new(1, 8, Some(10), SegmentState::Active);
    second.cursor = 9;
    let bars = project(
        &[(0, first), (1, second)].into(),
        Some(10),
        TransferStatus::Active,
    );
    assert_eq!(
        bars,
        [1.0, 1.0, 1.0, 0.2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.2, 1.0, 0.4, 0.0]
    );
}

#[tokio::test]
async fn redirect_hop_cap_allows_ten_hops_and_rejects_eleven() {
    let data = payload(4 * KIB, 14);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        if let Some(hop) = req
            .path
            .strip_prefix("/hop/")
            .and_then(|s| s.parse::<usize>().ok())
        {
            if hop > 0 {
                return Reply::redirect(&format!("/hop/{}", hop - 1));
            }
            return Reply::resource(req, &source, "\"hops\"");
        }
        Reply::empty("404 Not Found")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let ok_id = engine
        .add_http_download(request(server.url("/hop/10"), Some("ten.bin"), 1))
        .await
        .unwrap();
    completed(&engine, &ok_id).await;
    dir.assert_output("ten.bin", &data).await;

    let fail_id = engine
        .add_http_download(request(server.url("/hop/11"), Some("eleven.bin"), 1))
        .await
        .unwrap();
    let failed_snapshot = failed(&engine, &fail_id).await;
    assert!(failed_snapshot
        .error_message
        .unwrap()
        .contains("too many redirects"));
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn telemetry_batch_reports_live_speed_without_subscribers() {
    let data = payload(128 * KIB, 15);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        Reply::resource(req, &source, "\"batch\"").with_delay(Duration::from_millis(20))
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/batch.bin"), None, 2))
        .await
        .unwrap();
    let seen_speed = timeout(WAIT, async {
        loop {
            let batch = engine.telemetry_batch();
            if batch.active_count == 1 && batch.aggregate_down_bps > 0 {
                break batch;
            }
            sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(seen_speed.transfers.len(), 1);
    assert_eq!(seen_speed.transfers[0].id, id);
    assert!(seen_speed.active_threads >= 1);
    completed(&engine, &id).await;
    let idle = engine.telemetry_batch();
    assert_eq!(idle.active_count, 0);
    assert_eq!(idle.active_threads, 0);
    assert_eq!(idle.aggregate_down_bps, 0);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn validatorless_transfer_restarts_from_zero_on_resume() {
    let old = payload(512 * KIB, 16);
    let new = payload(512 * KIB, 17);
    let new_source = Arc::clone(&new);
    let switched = Arc::new(AtomicBool::new(false));
    let switch = Arc::clone(&switched);
    let server = Server::new(move |req| {
        let data = if switch.load(Ordering::SeqCst) {
            &new_source
        } else {
            &old
        };
        let mut reply = Reply::resource(req, data, "\"ignored\"");
        reply.headers.retain(|(name, _)| *name != "ETag");
        if !switch.load(Ordering::SeqCst) {
            reply = reply.with_delay(Duration::from_millis(8));
        }
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/no-validator.bin"), None, 2))
        .await
        .unwrap();
    wait_for(&engine, &id, |s| s.downloaded_bytes >= 64 * KIB as u64).await;
    engine.pause_transfer(&id).await.unwrap();
    assert!(!engine.snapshot(&id).unwrap().resumable);
    switched.store(true, Ordering::SeqCst);
    let earlier = server.requests().len();
    engine.resume_transfer(&id).await.unwrap();
    completed(&engine, &id).await;
    assert!(server.requests()[earlier..]
        .iter()
        .all(|req| req.header("if-range").is_none()));
    dir.assert_output("no-validator.bin", &new).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn probe_206_but_range_200_falls_back_to_single_stream() {
    let data = payload(128 * KIB, 18);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        if req.method == "HEAD" {
            return Reply::empty("405 Method Not Allowed");
        }
        if req.range() == Some(0..1) {
            return Reply::resource(req, &source, "\"range-probe\"");
        }
        let mut reply = Reply::resource(req, &source, "\"range-probe\"");
        reply.status = "200 OK";
        reply.content = Some((Arc::clone(&source), 0..source.len()));
        reply.headers = vec![
            ("ETag", "\"range-probe\"".into()),
            ("Content-Length", source.len().to_string()),
        ];
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/fallback.bin"), None, 4))
        .await
        .unwrap();
    let done = completed(&engine, &id).await;
    assert!(!done.resumable);
    assert_eq!(done.segments.len(), 1);
    dir.assert_output("fallback.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn checksum_failure_resets_progress_so_resume_recovers() {
    let valid = payload(96 * KIB, 19);
    let corrupt = payload(96 * KIB, 20);
    let valid_source = Arc::clone(&valid);
    let fixed = Arc::new(AtomicBool::new(false));
    let switch = Arc::clone(&fixed);
    let server = Server::new(move |req| {
        let data = if switch.load(Ordering::SeqCst) {
            &valid_source
        } else {
            &corrupt
        };
        Reply::resource(req, data, "\"sum\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let mut download = request(server.url("/sum.bin"), None, 2);
    download.expected_sha256 = Some(hex::encode(Sha256::digest(&*valid)));
    let id = engine.add_http_download(download).await.unwrap();
    let err_snap = failed(&engine, &id).await;
    assert_eq!(err_snap.sha256_verified, Some(false));
    assert_eq!(err_snap.downloaded_bytes, 0);

    fixed.store(true, Ordering::SeqCst);
    engine.resume_transfer(&id).await.unwrap();
    let ok_snap = completed(&engine, &id).await;
    assert_eq!(ok_snap.sha256_verified, Some(true));
    dir.assert_output("sum.bin", &valid).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn telemetry_stream_emits_terminal_idle_batch_on_completion() {
    let data = payload(64 * KIB, 21);
    let source = Arc::clone(&data);
    let (release, hold) = watch::channel(false);
    let server = Server::new(move |req| {
        let mut reply = Reply::resource(req, &source, "\"idle\"");
        if req.method == "GET" && req.range() != Some(0..1) {
            reply.body_gate = Some(hold.clone());
        }
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let mut rx = engine.subscribe_telemetry();
    let id = engine
        .add_http_download(request(server.url("/idle.bin"), None, 2))
        .await
        .unwrap();
    let idle = timeout(WAIT, async {
        let mut saw_active = false;
        loop {
            let batch = rx.recv().await.unwrap();
            if batch.active_count > 0 {
                if !saw_active {
                    saw_active = true;
                    release.send_replace(true);
                }
            } else if saw_active
                && batch.active_count == 0
                && batch
                    .transfers
                    .iter()
                    .any(|t| t.id == id && t.status == TransferStatus::Completed)
            {
                break batch;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(idle.active_threads, 0);
    assert_eq!(idle.aggregate_down_bps, 0);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn etag_rotation_during_probe_to_range_keeps_ranges_enabled() {
    let data = payload(128 * KIB, 22);
    let source = Arc::clone(&data);
    let rotated = AtomicBool::new(false);
    let server = Server::new(move |req| {
        if req.method == "HEAD" && !rotated.swap(true, Ordering::SeqCst) {
            return Reply::resource(req, &source, "\"v1\"");
        }
        Reply::resource(req, &source, "\"v2\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/rotate.bin"), None, 4))
        .await
        .unwrap();
    let done = completed(&engine, &id).await;
    assert!(done.resumable);
    assert_eq!(done.segments.len(), 4);
    dir.assert_output("rotate.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn weak_etag_merged_on_last_modified_resume_triggers_restart() {
    let old = payload(256 * KIB, 23);
    let new = payload(256 * KIB, 24);
    let new_source = Arc::clone(&new);
    let phase = Arc::new(AtomicUsize::new(0));
    let server_phase = Arc::clone(&phase);
    let server = Server::new(move |req| {
        if req.method == "HEAD" {
            let mut reply = if server_phase.load(Ordering::SeqCst) == 0 {
                Reply::resource(req, &old, "\"unused\"")
            } else {
                Reply::resource(req, &new_source, "\"v2\"")
            };
            reply.headers.retain(|(name, _)| *name != "ETag");
            reply
                .headers
                .push(("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT".into()));
            return reply;
        }
        if server_phase.load(Ordering::SeqCst) == 0 {
            let mut reply =
                Reply::resource(req, &old, "\"unused\"").with_delay(Duration::from_millis(8));
            reply.headers.retain(|(name, _)| *name != "ETag");
            reply
                .headers
                .push(("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT".into()));
            return reply;
        }
        let mut reply = Reply::resource(req, &new_source, "W/\"weak-new\"");
        reply
            .headers
            .push(("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT".into()));
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/weak-merge.bin"), None, 2))
        .await
        .unwrap();
    wait_for(&engine, &id, |s| s.downloaded_bytes >= 32 * KIB as u64).await;
    engine.pause_transfer(&id).await.unwrap();
    phase.store(1, Ordering::SeqCst);
    engine.resume_transfer(&id).await.unwrap();
    let done = completed(&engine, &id).await;
    assert_eq!(done.total_bytes, Some(new.len() as u64));
    dir.assert_output("weak-merge.bin", &new).await;
    engine.shutdown().await.unwrap();
}

#[test]
fn chained_singleton_wildcards_respect_group_cap_and_inplace_expansion() {
    let ok = expand_wildcard_url("https://example.com/[1-2]/[1-1]/[a-b].zip").unwrap();
    assert_eq!(
        ok,
        [
            "https://example.com/1/1/a.zip",
            "https://example.com/1/1/b.zip",
            "https://example.com/2/1/a.zip",
            "https://example.com/2/1/b.zip",
        ]
    );
    let many_singletons = "[1-1]".repeat(40);
    let abuse = format!("https://example.com/[1-10000]/{many_singletons}");
    assert!(expand_wildcard_url(&abuse).is_err());
}
