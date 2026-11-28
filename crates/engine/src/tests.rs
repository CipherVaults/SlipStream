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

#[tokio::test]
async fn phase4_mirrors_fail_over_without_cross_origin_credentials() {
    let primary = Server::new(|_| Reply::empty("404 Not Found")).await;
    let forbidden = Server::new(|_| Reply::empty("403 Forbidden")).await;
    let unavailable = Server::new(|_| Reply::empty("503 Service Unavailable")).await;
    let data = payload(2 * CHUNK, 42);
    let mirror_data = data.clone();
    let mirror =
        Server::new(move |request| Reply::resource(request, &mirror_data, "\"mirror\"")).await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let mut request = request(primary.url("/primary"), Some("mirror.bin"), 2);
    request.mirror_urls = vec![
        forbidden.url("/forbidden"),
        unavailable.url("/unavailable"),
        mirror.url("/file"),
    ];
    request
        .headers
        .insert("Authorization".into(), "Bearer only-primary".into());
    request
        .headers
        .insert("Cookie".into(), "session=only-primary".into());
    request
        .headers
        .insert("X-Api-Key".into(), "only-primary".into());
    request.expected_size_bytes = Some(data.len() as u64);
    request.expected_sha256 = Some(hex::encode(Sha256::digest(&*data)));
    let id = engine.add_http_download(request).await.unwrap();
    wait_for(&engine, &id, |snapshot| {
        snapshot.status == TransferStatus::Completed
    })
    .await;
    dir.assert_output("mirror.bin", &data).await;
    assert!(primary
        .requests()
        .iter()
        .any(|request| request.header("Authorization").is_some()));
    for server in [&forbidden, &unavailable, &mirror] {
        let requests = server.requests();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|request| {
            ["Authorization", "Cookie", "X-Api-Key"]
                .into_iter()
                .all(|name| request.header(name).is_none())
        }));
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn phase4_sqlite_mirror_urls_round_trip_and_default_for_legacy_rows() {
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let mut transfer = request("https://example.org/file".into(), Some("file"), 1);
    transfer.start_paused = true;
    let mirror = "https://mirror.example.org/file";
    transfer.mirror_urls = vec![mirror.into()];
    let id = engine.add_http_download(transfer).await.unwrap();
    engine.shutdown().await.unwrap();
    let db = Database::open(&config.db_path).unwrap();
    assert_eq!(db.transfers().unwrap()[0].0.mirror_urls, [mirror]);
    drop(db);

    let db = rusqlite::Connection::open(&config.db_path).unwrap();
    let json: String = db
        .query_row("SELECT record FROM transfers WHERE id = ?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&json).unwrap();
    let fields = json.as_object_mut().unwrap();
    fields.remove("mirror_urls");
    fields.remove("representation_source");
    fields.remove("expected_size_bytes");
    db.execute(
        "UPDATE transfers SET record = ?1 WHERE id = ?2",
        [json.to_string(), id],
    )
    .unwrap();
    drop(db);
    let db = Database::open(&config.db_path).unwrap();
    assert!(db.transfers().unwrap()[0].0.mirror_urls.is_empty());
}

#[tokio::test]
async fn phase4_proxy_configuration_applies_authentication_and_bypass() {
    let data = payload(CHUNK, 55);
    let proxied_data = data.clone();
    let proxy =
        Server::new(move |request| Reply::resource(request, &proxied_data, "\"proxied\"")).await;
    let bypass_data = data.clone();
    let origin =
        Server::new(move |request| Reply::resource(request, &bypass_data, "\"direct\"")).await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    engine
        .set_proxy_config(EngineProxyConfig::Manual {
            url: proxy.url.clone(),
            username: Some("u".into()),
            password: Some("p".into()),
            bypass_hosts: vec!["127.0.0.1".into()],
        })
        .unwrap();
    for (url, name) in [
        ("http://example.invalid/file".into(), "proxied.bin"),
        (origin.url("/file"), "direct.bin"),
    ] {
        let id = engine
            .add_http_download(request(url, Some(name), 1))
            .await
            .unwrap();
        wait_for(&engine, &id, |snapshot| {
            snapshot.status == TransferStatus::Completed
        })
        .await;
        dir.assert_output(name, &data).await;
    }
    let requests = proxy.requests();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| {
        request.path == "http://example.invalid/file"
            && request.header("Proxy-Authorization") == Some("Basic dTpw")
    }));
    assert!(origin
        .requests()
        .iter()
        .all(|request| request.header("Proxy-Authorization").is_none()));
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn phase4_private_proxy_hostname_does_not_allow_bypassed_connections() {
    let data = payload(CHUNK, 77);
    let data_for_proxy = data.clone();
    let proxy =
        Server::new(move |request| Reply::resource(request, &data_for_proxy, "\"proxy\"")).await;
    let proxy_url = proxy.url.replace("127.0.0.1", "localhost");
    let dir = TestDir::new().await;
    let mut config = dir.config();
    config.allow_loopback = false;
    let engine = DownloadEngine::new_with_proxy(
        config,
        EngineProxyConfig::Manual {
            url: proxy_url.clone(),
            username: None,
            password: None,
            bypass_hosts: vec!["localhost".into()],
        },
    )
    .await
    .unwrap();
    let id = engine
        .add_http_download(request(
            "http://example.invalid/file".into(),
            Some("proxied.bin"),
            1,
        ))
        .await
        .unwrap();
    completed(&engine, &id).await;
    dir.assert_output("proxied.bin", &data).await;
    let proxied_requests = proxy.requests().len();
    assert!(proxied_requests > 0);

    let id = engine
        .add_http_download(request(
            format!("{proxy_url}/blocked"),
            Some("blocked.bin"),
            1,
        ))
        .await
        .unwrap();
    let snapshot = failed(&engine, &id).await;
    assert!(snapshot
        .error_message
        .unwrap()
        .contains("blocked connection"));
    assert_eq!(proxy.requests().len(), proxied_requests);
    engine.shutdown().await.unwrap();
}

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
        self.headers
            .iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
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
    connection_slot: Option<tokio::sync::OwnedSemaphorePermit>,
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
            connection_slot: None,
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
        let _connection_slot = self.connection_slot;
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
        Self::with_listener(listener, reply)
    }

    fn with_listener(
        listener: TcpListener,
        reply: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    ) -> Self {
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
        headers.insert(key.to_owned(), value.trim().into());
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
        expected_size_bytes: None,
        start_paused: false,
        overwrite_existing: false,
        ..AddHttpDownloadRequest::default()
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
                record
                    .headers
                    .insert("Authorization".into(), "private".into());
                record.referrer = Some("https://example.com/referrer".into());
                record.source_url =
                    "https://user:pass@example.com/file?token=private#fragment".into();
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
    let (record, _) = engine
        .inner
        .lock()
        .db()
        .unwrap()
        .transfers()
        .unwrap()
        .into_iter()
        .find(|(record, _)| record.id == renamed)
        .unwrap();
    assert!(record.headers.is_empty());
    assert_eq!(record.referrer, None);
    assert_eq!(record.source_url, "https://example.com/file");
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
async fn completed_downloads_scrub_credentials_and_publish_under_unique_names() {
    let data = payload(8 * KIB, 6);
    let contents = data.clone();
    let server = Server::new(move |req| Reply::resource(req, &contents, "\"duplicates\"")).await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let mut download = request(server.url("/repeated.bin?token=private#fragment"), None, 1);
    download.headers = [("X-Api-Key".into(), "private".into())].into();
    download.referrer = Some("https://example.com/path?token=private".into());
    let first = engine.add_http_download(download.clone()).await.unwrap();
    let second = engine.add_http_download(download).await.unwrap();
    let mut filenames = vec![
        completed(&engine, &first).await.filename,
        completed(&engine, &second).await.filename,
    ];
    filenames.sort_unstable();
    assert_eq!(filenames, ["repeated (1).bin", "repeated.bin"]);
    for name in filenames {
        dir.assert_output(&name, &data).await;
    }
    engine.shutdown().await.unwrap();

    let records = Database::open(&config.db_path)
        .unwrap()
        .transfers()
        .unwrap();
    assert_eq!(records.len(), 2);
    for (record, _) in records {
        assert_eq!(record.status, TransferStatus::Completed);
        assert!(record.headers.is_empty());
        assert_eq!(record.referrer, None);
        assert_eq!(record.source_url, server.url("/repeated.bin"));
        assert!(config.default_download_dir.join(record.filename).is_file());
    }
}

#[tokio::test]
async fn database_load_skips_a_corrupt_transfer_record() {
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/valid", listener.local_addr().unwrap());
    let valid = engine
        .add_http_download(request(url, None, 1))
        .await
        .unwrap();
    engine.shutdown().await.unwrap();
    let raw = rusqlite::Connection::open(&config.db_path).unwrap();
    raw.execute(
        "INSERT INTO transfers (id, queue_id, record) VALUES ('broken', 'default', '{')",
        [],
    )
    .unwrap();
    let records = Database::open(&config.db_path)
        .unwrap()
        .transfers()
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0.id, valid);
}

#[test]
fn numbered_output_names_handle_extensionless_and_long_unicode_names() {
    assert_eq!(output::numbered_filename("download", 1), "download (1)");
    let filename = format!("{}.bin", "界".repeat(65));
    let deduplicated = output::numbered_filename(&filename, 1);
    assert!(deduplicated.ends_with(" (1).bin"));
    assert!(slipstream_protocol::validate_safe_filename_component(&deduplicated).is_ok());
}

#[tokio::test]
async fn paused_additions_persist_without_connecting_until_resumed() {
    let data = payload(KIB, 1);
    let contents = data.clone();
    let server = Server::new(move |req| Reply::resource(req, &contents, "\"paused\"")).await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let mut ids = Vec::new();
    for request_paused in [true, false] {
        let mut download = request(server.url("/paused.bin"), None, 1);
        download.start_paused = request_paused;
        download.expected_size_bytes = Some(data.len() as u64);
        let id = engine
            .add_http_download_with_options(
                download,
                ExtendedHttpOptions {
                    start_paused: !request_paused,
                    description: Some("Save for later".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(engine.snapshot(&id).unwrap().status, TransferStatus::Paused);
        ids.push(id);
    }
    sleep(Duration::from_millis(100)).await;
    assert!(server.requests().is_empty());
    engine.shutdown().await.unwrap();
    let engine = DownloadEngine::new(config).await.unwrap();
    for id in &ids {
        let snapshot = engine.snapshot(id).unwrap();
        assert_eq!(snapshot.status, TransferStatus::Paused);
        assert_eq!(snapshot.total_bytes, Some(data.len() as u64));
        assert_eq!(snapshot.description.as_deref(), Some("Save for later"));
        assert_eq!(snapshot.connect_rtt_ms, None);
    }
    engine.resume_transfer(&ids[0]).await.unwrap();
    let snapshot = completed(&engine, &ids[0]).await;
    assert!(snapshot.connect_rtt_ms.is_some());
    assert_eq!(
        engine.snapshot(&ids[1]).unwrap().status,
        TransferStatus::Paused
    );
    dir.assert_output("paused.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_matching_uses_url_or_canonical_path_and_ignores_canceled() {
    let data = payload(KIB, 1);
    let server = Server::new(move |req| Reply::resource(req, &data, "\"duplicate\"")).await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let mut download = request(server.url("/download?token=test#old"), Some("File.bin"), 1);
    download.start_paused = true;
    let id = engine.add_http_download(download.clone()).await.unwrap();
    let root = &config.default_download_dir;
    let same_url = server.url("/download?token=test#new");
    assert_eq!(
        engine
            .find_duplicate(&same_url, root, "other.bin")
            .unwrap()
            .transfer_id,
        id
    );
    let filename = if cfg!(windows) {
        "file.BIN"
    } else {
        "File.bin"
    };
    assert_eq!(
        engine
            .find_duplicate(&server.url("/another"), &root.join("."), filename)
            .unwrap()
            .transfer_id,
        id
    );
    assert!(engine
        .find_duplicate(&server.url("/download?token=another"), root, "other.bin")
        .is_none());
    engine.cancel_transfer(&id).await.unwrap();
    assert!(engine.find_duplicate(&same_url, root, "File.bin").is_none());
    download.start_paused = false;
    let id = engine.add_http_download(download).await.unwrap();
    let finished = completed(&engine, &id).await;
    assert_eq!(finished.source_url, server.url("/download"));
    engine.shutdown().await.unwrap();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let duplicate = engine.find_duplicate(&same_url, root, "other.bin").unwrap();
    assert_eq!(duplicate.transfer_id, id);
    assert_eq!(duplicate.status, TransferStatus::Completed);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn overwrite_atomically_replaces_files_and_rejects_unsafe_targets() {
    let data = payload(KIB, 1);
    let contents = data.clone();
    let server = Server::new(move |req| Reply::resource(req, &contents, "\"overwrite\"")).await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let root = &config.default_download_dir;
    std::fs::create_dir(root).unwrap();
    std::fs::write(root.join("existing.bin"), b"old contents").unwrap();
    std::fs::create_dir(root.join("directory.bin")).unwrap();
    let outside = dir.path.join("protected.bin");
    std::fs::write(&outside, b"protected contents").unwrap();
    let link = root.join("link.bin");
    #[cfg(unix)]
    let symlink = std::os::unix::fs::symlink(&outside, &link);
    #[cfg(windows)]
    let symlink = std::os::windows::fs::symlink_file(&outside, &link);
    let mut unsafe_targets = vec!["directory.bin"];
    match symlink {
        Ok(()) => unsafe_targets.push("link.bin"),
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(1314) => {}
        Err(error) => panic!("could not create a test symlink: {error}"),
    }
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let id = engine
        .add_http_download_with_options(
            request(server.url("/replace"), Some("existing.bin"), 1),
            ExtendedHttpOptions {
                overwrite_existing: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(completed(&engine, &id).await.filename, "existing.bin");
    dir.assert_output("existing.bin", &data).await;
    assert!(!root.join("existing (1).bin").exists());

    for filename in unsafe_targets {
        let mut download = request(server.url("/replace"), Some(filename), 1);
        download.overwrite_existing = true;
        let id = engine.add_http_download(download).await.unwrap();
        assert!(failed(&engine, &id)
            .await
            .error_message
            .unwrap()
            .contains("regular download file"));
    }
    assert!(root.join("directory.bin").is_dir());
    assert_eq!(std::fs::read(&outside).unwrap(), b"protected contents");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn overwrites_leave_only_the_latest_completed_transfer_owning_the_output() {
    let original = payload(KIB, 7);
    let replacement = payload(KIB, 8);
    let first = original.clone();
    let next = replacement.clone();
    let server = Server::new(move |req| {
        let contents = if req.path == "/first" { &first } else { &next };
        Reply::resource(req, contents, "\"ownership\"")
    })
    .await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    let mut download = request(server.url("/first"), Some("shared.bin"), 1);
    download.expected_sha256 = Some(hex::encode(Sha256::digest(&*original)));
    let old_id = engine.add_http_download(download).await.unwrap();
    let old = completed(&engine, &old_id).await;
    assert_eq!(old.sha256_verified, Some(true));
    assert!(!old.output_superseded);

    let filename = if cfg!(windows) {
        "SHARED.bin"
    } else {
        "shared.bin"
    };
    let mut download = request(server.url("/replacement"), Some(filename), 1);
    download.save_dir = Some(
        config
            .default_download_dir
            .join(".")
            .to_string_lossy()
            .into_owned(),
    );
    download.overwrite_existing = true;
    download.expected_sha256 = Some(hex::encode(Sha256::digest(&*replacement)));
    let (left, right) = tokio::join!(
        engine.add_http_download(download.clone()),
        engine.add_http_download(download),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    completed(&engine, &left).await;
    completed(&engine, &right).await;
    dir.assert_output(filename, &replacement).await;
    let assert_ownership = |engine: &DownloadEngine| {
        let old = engine.snapshot(&old_id).unwrap();
        assert_eq!(old.status, TransferStatus::Completed);
        assert_eq!(old.sha256_verified, None);
        assert!(old.output_superseded);
        let left = engine.snapshot(&left).unwrap();
        let right = engine.snapshot(&right).unwrap();
        assert_ne!(left.output_superseded, right.output_superseded);
        for record in [left, right] {
            assert_eq!(
                record.sha256_verified,
                (!record.output_superseded).then_some(true)
            );
        }
    };
    assert_ownership(&engine);
    engine.shutdown().await.unwrap();
    let engine = DownloadEngine::new(config).await.unwrap();
    assert_ownership(&engine);
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
            let mut reply = Reply::redirect(&destination);
            reply
                .headers
                .push(("Set-Cookie", "session=origin-only".into()));
            reply
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
            ..QueueConfig::default()
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
async fn sends_default_http_headers_and_preserves_caller_values() {
    let data = payload(KIB, 12);
    let server = Server::new(move |req| Reply::resource(req, &data, "\"headers\"")).await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let default_id = engine
        .add_http_download(request(server.url("/default"), None, 1))
        .await
        .unwrap();
    completed(&engine, &default_id).await;
    let mut custom = request(server.url("/custom"), None, 1);
    custom
        .headers
        .insert("uSeR-aGeNt".into(), "Fixture Player/1.0".into());
    custom.headers.insert("Accept".into(), "video/mp4".into());
    custom
        .headers
        .insert("accept-language".into(), "ar-EG".into());
    let custom_id = engine.add_http_download(custom).await.unwrap();
    completed(&engine, &custom_id).await;

    let requests = server.requests();
    for (path, accept, user_agent_prefix, language) in [
        ("/default", "*/*", "Mozilla/5.0 ", "en-US,en;q=0.9"),
        ("/custom", "video/mp4", "Fixture Player/1.0", "ar-EG"),
    ] {
        let matching: Vec<_> = requests.iter().filter(|req| req.path == path).collect();
        assert!(matching.iter().any(|req| req.method == "GET"));
        for req in matching {
            assert!(req.headers.contains_key("Accept-Language"));
            assert_eq!(req.header("accept"), Some(accept));
            assert_eq!(req.header("accept-language"), Some(language));
            assert_eq!(req.header("accept-encoding"), Some("identity"));
            let user_agent = req.header("user-agent").unwrap();
            if path == "/custom" {
                assert_eq!(user_agent, user_agent_prefix);
            } else {
                assert!(user_agent.starts_with(user_agent_prefix));
            }
        }
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn retries_rejected_range_probes_with_one_plain_get() {
    let data = payload(4 * KIB, 13);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        if req.method == "HEAD" || req.header("range").is_some() {
            let status = match req.path.as_str() {
                "/400" => "400 Bad Request",
                "/403" | "/206" | "/partial" => "403 Forbidden",
                "/405" => "405 Method Not Allowed",
                "/412" => "412 Precondition Failed",
                "/416" => "416 Range Not Satisfiable",
                "/501" => "501 Not Implemented",
                _ => unreachable!(),
            };
            let mut reply = Reply::empty(status);
            if req.path == "/416" {
                reply
                    .headers
                    .push(("Content-Range", format!("bytes */{}", source.len())));
            }
            return reply;
        }
        let mut reply = Reply::resource(req, &source, "\"signed-cdn\"");
        if matches!(req.path.as_str(), "/206" | "/partial") {
            let length = source.len();
            let total = if req.path == "/206" {
                length
            } else {
                length + 1
            };
            reply.status = "206 Partial Content";
            reply
                .headers
                .push(("Content-Range", format!("bytes 0-{}/{total}", length - 1)));
        }
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    for path in ["/400", "/403", "/405", "/412", "/416", "/501", "/206"] {
        let id = engine
            .add_http_download(request(server.url(path), None, 4))
            .await
            .unwrap();
        let done = completed(&engine, &id).await;
        assert_eq!(done.total_bytes, Some(data.len() as u64));
        assert_eq!(done.segments.len(), 1);
        assert!(!done.resumable);
        dir.assert_output(&done.filename, &data).await;
        let requests = server.requests();
        let methods_and_ranges: Vec<_> = requests
            .iter()
            .filter(|req| req.path == path)
            .map(|req| (req.method.as_str(), req.header("range")))
            .collect();
        assert_eq!(
            methods_and_ranges,
            [("HEAD", None), ("GET", Some("bytes=0-0")), ("GET", None),]
        );
    }
    let partial = engine
        .add_http_download(request(server.url("/partial"), None, 4))
        .await
        .unwrap();
    let failed = failed(&engine, &partial).await;
    assert!(failed
        .error_message
        .unwrap()
        .contains("incomplete response to plain GET"));
    assert!(!dir.config().default_download_dir.join("partial").exists());
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn backs_down_when_more_than_two_connections_are_rejected() {
    let data = payload(1024 * KIB, 23);
    let source = Arc::clone(&data);
    let slots = Arc::new(tokio::sync::Semaphore::new(2));
    let rejected = Arc::new(AtomicUsize::new(0));
    let rejections = Arc::clone(&rejected);
    let server = Server::new(move |req| {
        if req.method == "HEAD" {
            return Reply::resource(req, &source, "\"backdown\"");
        }
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            rejections.fetch_add(1, Ordering::SeqCst);
            return Reply::empty("503 Service Unavailable");
        };
        let mut reply = Reply::resource(req, &source, "\"backdown\"");
        reply.connection_slot = Some(slot);
        reply.initial_delay = Duration::from_millis(75);
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/backdown.bin"), None, 16))
        .await
        .unwrap();
    let done = completed(&engine, &id).await;
    assert!(rejected.load(Ordering::SeqCst) > 0);
    assert_eq!(done.segments.len(), 16);
    assert!(done
        .segments
        .iter()
        .all(|segment| segment.state == SegmentState::Completed));
    assert!(server
        .requests()
        .iter()
        .filter(|req| req.method == "GET")
        .all(|req| req.range().is_some()));
    dir.assert_output("backdown.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn backs_down_and_completes_when_server_rejects_extra_connections_with_http_460() {
    let data = payload(512 * KIB, 29);
    let source = Arc::clone(&data);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let rejected = Arc::new(AtomicUsize::new(0));
    let rejections = Arc::clone(&rejected);
    let server = Server::new(move |req| {
        if req.method == "HEAD" {
            return Reply::resource(req, &source, "\"limit-460\"");
        }
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            rejections.fetch_add(1, Ordering::SeqCst);
            return Reply::empty("460 Connection Limit Exceeded");
        };
        let mut reply = Reply::resource(req, &source, "\"limit-460\"");
        reply.connection_slot = Some(slot);
        reply.initial_delay = Duration::from_millis(60);
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/limit460.bin"), None, 16))
        .await
        .unwrap();
    let done = completed(&engine, &id).await;
    assert!(rejected.load(Ordering::SeqCst) > 0);
    assert!(done
        .segments
        .iter()
        .all(|segment| segment.state == SegmentState::Completed));
    dir.assert_output("limit460.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn backs_down_on_custom_status_learns_host_cap_and_surfaces_404_on_dead_link() {
    let data = payload(512 * KIB, 31);
    let source = Arc::clone(&data);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let rejected = Arc::new(AtomicUsize::new(0));
    let rejections = Arc::clone(&rejected);
    let server = Server::new(move |req| {
        if req.path == "/dead.bin" {
            return Reply::empty("404 Not Found");
        }
        if req.method == "HEAD" {
            return Reply::resource(req, &source, "\"custom-cap\"");
        }
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            rejections.fetch_add(1, Ordering::SeqCst);
            return Reply::empty("477 Custom Limit");
        };
        let mut reply = Reply::resource(req, &source, "\"custom-cap\"");
        reply.connection_slot = Some(slot);
        reply.initial_delay = Duration::from_millis(50);
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let first = engine
        .add_http_download(request(server.url("/first.bin"), None, 16))
        .await
        .unwrap();
    let _ = completed(&engine, &first).await;
    assert!(rejected.load(Ordering::SeqCst) > 0);
    dir.assert_output("first.bin", &data).await;

    let before_second = rejected.load(Ordering::SeqCst);
    let second = engine
        .add_http_download(request(server.url("/second.bin"), None, 16))
        .await
        .unwrap();
    let _ = completed(&engine, &second).await;
    assert_eq!(
        rejected.load(Ordering::SeqCst),
        before_second,
        "learned host cap should prevent blasting 16 workers on the next download"
    );
    dir.assert_output("second.bin", &data).await;

    let dead = engine
        .add_http_download(request(server.url("/dead.bin"), None, 4))
        .await
        .unwrap();
    let failed_snapshot = failed(&engine, &dead).await;
    assert!(failed_snapshot
        .error_message
        .as_deref()
        .is_some_and(|msg| msg.contains("404")));
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn single_worker_falls_back_to_full_get_on_http_460_range_rejection() {
    let data = payload(64 * KIB, 37);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        if req.range().is_some() {
            return Reply::empty("460 Connection Limit Exceeded");
        }
        Reply::resource(req, &source, "\"range-460\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/single460.bin"), None, 1))
        .await
        .unwrap();
    let _ = completed(&engine, &id).await;
    dir.assert_output("single460.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn restarts_rejected_single_worker_ranges_with_one_full_get() {
    let data = payload(64 * KIB, 28);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        if req.range().is_some() {
            return Reply::empty(match req.path.as_str() {
                "/403" => "403 Forbidden",
                "/503" => "503 Service Unavailable",
                "/416" => "416 Range Not Satisfiable",
                _ => unreachable!(),
            });
        }
        Reply::resource(req, &source, "\"full-only\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    for (path, range_attempts) in [("/403", 2), ("/503", 1), ("/416", 1)] {
        let id = engine
            .add_http_download(request(server.url(path), None, 1))
            .await
            .unwrap();
        let done = completed(&engine, &id).await;
        assert!(!done.resumable);
        let requests = server.requests();
        let gets: Vec<_> = requests
            .iter()
            .filter(|req| req.path == path && req.method == "GET")
            .collect();
        assert_eq!(gets.len(), range_attempts + 1);
        assert!(gets[..range_attempts]
            .iter()
            .all(|req| req.range().is_some()));
        assert!(gets[range_attempts].range().is_none());
        dir.assert_output(&done.filename, &data).await;
    }
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_ranges_after_a_socket_drop_wait_for_a_successful_full_get() {
    let data = payload(64 * KIB, 29);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| match req.range() {
        Some(range) if range.start == 0 => {
            let mut reply = Reply::resource(req, &source, "\"restart\"");
            reply.cutoff = Some(CHUNK);
            reply
        }
        Some(_) if req.path == "/503" => Reply::empty("503 Service Unavailable"),
        Some(_) => Reply::empty("403 Forbidden"),
        None if req.method == "GET" && req.path == "/failed" => Reply::empty("410 Gone"),
        None => Reply::resource(req, &source, "\"restart\""),
    })
    .await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    for path in ["/403", "/503"] {
        let id = engine
            .add_http_download(request(server.url(path), None, 1))
            .await
            .unwrap();
        let done = completed(&engine, &id).await;
        dir.assert_output(&done.filename, &data).await;
    }
    let id = engine
        .add_http_download(request(server.url("/failed"), None, 1))
        .await
        .unwrap();
    let snapshot = failed(&engine, &id).await;
    assert!(snapshot.error_message.unwrap().contains("410 Gone"));
    assert_eq!(snapshot.downloaded_bytes, CHUNK as u64);
    let (part, final_path) = engine
        .inner
        .lock()
        .job(&id)
        .unwrap()
        .record
        .paths()
        .unwrap();
    let bytes = tokio::fs::read(part).await.unwrap();
    assert_eq!(bytes.len(), data.len());
    assert_eq!(&bytes[..CHUNK], &data[..CHUNK]);
    assert!(!final_path.exists());

    let requests = server.requests();
    for path in ["/403", "/503", "/failed"] {
        let gets: Vec<_> = requests
            .iter()
            .filter(|req| req.method == "GET" && req.path == path)
            .map(|req| req.range().map(|range| range.start))
            .collect();
        assert_eq!(gets, [Some(0), Some(CHUNK), None]);
    }
    engine.shutdown().await.unwrap();
    let reopened = DownloadEngine::new(config).await.unwrap();
    assert_eq!(
        reopened.snapshot(&id).unwrap().downloaded_bytes,
        CHUNK as u64
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn streams_the_only_get_allowed_by_a_single_use_url() {
    let data = payload(64 * KIB, 24);
    let source = Arc::clone(&data);
    let gets = AtomicUsize::new(0);
    let server = Server::new(move |req| {
        if req.method == "HEAD" {
            return Reply::empty("403 Forbidden");
        }
        if gets.fetch_add(1, Ordering::SeqCst) != 0 {
            return Reply::empty("410 Gone");
        }
        let mut reply = Reply::resource(req, &source, "\"one-use\"");
        reply.status = "200 OK";
        reply
            .headers
            .retain(|(name, _)| !matches!(*name, "Content-Range" | "Content-Length"));
        reply
            .headers
            .push(("Content-Length", source.len().to_string()));
        reply.content = Some((Arc::clone(&source), 0..source.len()));
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/one-use.bin"), None, 16))
        .await
        .unwrap();
    completed(&engine, &id).await;
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|req| req.method == "GET")
            .count(),
        1
    );
    dir.assert_output("one-use.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn forbidden_cookie_and_hotlink_headers_fall_back_to_winning_profiles() {
    let data = payload(128 * KIB, 25);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        let cookie = req.header("cookie").is_some();
        let hotlink = req.header("referer").is_some() || req.header("origin").is_some();
        if (req.path == "/cookie" && cookie)
            || (req.path == "/hotlink" && req.method == "GET" && (cookie || hotlink))
        {
            return Reply::empty("403 Forbidden");
        }
        Reply::resource(req, &source, "\"headers\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    for (path, workers) in [("/cookie", 4), ("/hotlink", 1)] {
        let mut download = request(server.url(path), None, workers);
        download.headers = [
            ("Cookie".into(), "session=stale".into()),
            ("Referer".into(), "https://example.com/watch".into()),
            ("Origin".into(), "https://example.com".into()),
            ("User-Agent".into(), "Fixture Player".into()),
            ("X-Custom".into(), "browser".into()),
        ]
        .into();
        let id = engine.add_http_download(download).await.unwrap();
        let done = completed(&engine, &id).await;
        dir.assert_output(&done.filename, &data).await;
    }
    let requests = server.requests();
    let cookie: Vec<_> = requests
        .iter()
        .filter(|req| req.path == "/cookie")
        .collect();
    assert_eq!(
        cookie
            .iter()
            .filter(|req| req.method == "HEAD" && req.header("cookie").is_some())
            .count(),
        2
    );
    let winning_head = cookie
        .iter()
        .position(|req| req.method == "HEAD" && req.header("cookie").is_none())
        .unwrap();
    assert!(cookie[winning_head..].iter().all(|req| {
        req.header("cookie").is_none()
            && req.header("referer") == Some("https://example.com/watch")
            && req.header("user-agent") == Some("Fixture Player")
    }));

    let hotlink: Vec<_> = requests
        .iter()
        .filter(|req| req.path == "/hotlink" && req.method == "GET")
        .collect();
    let [default, ipv4, no_cookie, bare] = hotlink.as_slice() else {
        panic!("expected four GET profiles: {hotlink:?}");
    };
    assert_eq!(default.header("cookie"), Some("session=stale"));
    assert_eq!(ipv4.header("cookie"), Some("session=stale"));
    assert!(ipv4.headers.contains_key("User-Agent"));
    assert_eq!(no_cookie.header("cookie"), None);
    assert_eq!(
        no_cookie.header("referer"),
        Some("https://example.com/watch")
    );
    for name in ["cookie", "referer", "origin", "x-custom"] {
        assert_eq!(bare.header(name), None);
    }
    assert_eq!(bare.header("user-agent"), Some("Fixture Player"));
    assert_eq!(bare.header("accept"), Some("*/*"));
    assert_eq!(bare.header("accept-language"), Some("en-US,en;q=0.9"));
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn ipv6_forbidden_responses_can_still_retry_without_cookie_or_referer() {
    let data = payload(16 * KIB, 32);
    let source = Arc::clone(&data);
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let server = Server::with_listener(listener, move |req| {
        if req.method == "GET"
            && (req.path == "/forbidden"
                || req.header("cookie").is_some()
                || req.header("referer").is_some())
        {
            return Reply::empty("403 Forbidden");
        }
        Reply::resource(req, &source, "\"ipv6\"")
    });
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let mut download = request(server.url("/ipv6"), None, 1);
    download
        .headers
        .insert("Cookie".into(), "session=stale".into());
    download.referrer = Some("https://example.com/watch".into());
    let id = engine.add_http_download(download).await.unwrap();
    completed(&engine, &id).await;
    let requests = server.requests();
    let gets: Vec<_> = requests.iter().filter(|req| req.method == "GET").collect();
    let [with_cookie, without_cookie, bare] = gets.as_slice() else {
        panic!("expected three GET profiles: {gets:?}");
    };
    assert_eq!(with_cookie.header("cookie"), Some("session=stale"));
    assert_eq!(without_cookie.header("cookie"), None);
    assert!(without_cookie.header("referer").is_some());
    assert_eq!(bare.header("referer"), None);
    dir.assert_output("ipv6", &data).await;

    let id = engine
        .add_http_download(request(server.url("/forbidden"), None, 1))
        .await
        .unwrap();
    assert_eq!(
        failed(&engine, &id).await.error_message.as_deref(),
        Some("HTTP 403 Forbidden")
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn retries_429_after_the_server_delay() {
    let data = payload(64 * KIB, 26);
    let source = Arc::clone(&data);
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let times = Arc::clone(&attempts);
    let server = Server::new(move |req| {
        if req.method == "GET" {
            let mut times = times.lock().unwrap();
            times.push(Instant::now());
            if times.len() == 1 {
                let mut reply = Reply::empty("429 Too Many Requests");
                reply.headers.push(("Retry-After", "1".into()));
                return reply;
            }
        }
        Reply::resource(req, &source, "\"retry-after\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/throttled.bin"), None, 1))
        .await
        .unwrap();
    completed(&engine, &id).await;
    {
        let times = attempts.lock().unwrap();
        assert_eq!(times.len(), 2);
        assert!(times[1] - times[0] >= Duration::from_secs(1));
    }
    dir.assert_output("throttled.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn range_probe_503_retries_the_range_after_the_server_delay() {
    let data = payload(16 * KIB, 31);
    let source = Arc::clone(&data);
    let times = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::clone(&times);
    let server = Server::new(move |req| {
        if req.method == "HEAD" {
            return Reply::empty("405 Method Not Allowed");
        }
        if req.header("range") == Some("bytes=0-0") {
            let mut attempts = attempts.lock().unwrap();
            attempts.push(Instant::now());
            if attempts.len() == 1 {
                let mut reply = Reply::empty("503 Service Unavailable");
                reply.headers.push(("Retry-After", "1".into()));
                return reply;
            }
        }
        Reply::resource(req, &source, "\"range-retry\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let id = engine
        .add_http_download(request(server.url("/range-retry"), None, 1))
        .await
        .unwrap();
    completed(&engine, &id).await;
    {
        let times = times.lock().unwrap();
        assert_eq!(times.len(), 2);
        assert!(times[1] - times[0] >= Duration::from_secs(1));
    }
    assert!(server
        .requests()
        .iter()
        .filter(|req| req.method == "GET")
        .all(|req| req.range().is_some()));
    dir.assert_output("range-retry", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn keeps_redirect_cookies_on_the_same_origin_and_in_the_same_chain() {
    let data = payload(8 * KIB, 27);
    let source = Arc::clone(&data);
    let server = Server::new(move |req| {
        if req.path == "/start" {
            let mut reply = Reply::redirect("/final/item");
            reply.headers.extend([
                ("Set-Cookie", "session=new; HttpOnly".into()),
                (
                    "Set-Cookie",
                    "challenge=passed; pAtH=/final; Domain=.127.0.0.1".into(),
                ),
                ("Set-Cookie", "removed=; Max-Age=0; Path=/".into()),
                (
                    "Set-Cookie",
                    "expired=dead; Expires=Thu, 01 Jan 1970 00:00:00 GMT".into(),
                ),
                ("Set-Cookie", "secure=secret; sEcUrE".into()),
                ("Set-Cookie", "other=secret; Path=/other".into()),
                ("Set-Cookie", "wrong-prefix=secret; Path=/fin".into()),
                (
                    "Set-Cookie",
                    "wrong-domain=secret; Domain=example.com".into(),
                ),
            ]);
            return reply;
        }
        Reply::resource(req, &source, "\"redirect-cookie\"")
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    let mut download = request(server.url("/start"), Some("cookie.bin"), 1);
    download.headers.insert(
        "Cookie".into(),
        "pref=dark; session=old; removed=old; expired=old".into(),
    );
    let id = engine.add_http_download(download).await.unwrap();
    completed(&engine, &id).await;
    for req in server.requests() {
        let expected = if req.path == "/start" {
            "pref=dark; session=old; removed=old; expired=old"
        } else {
            "pref=dark; session=new; challenge=passed"
        };
        assert_eq!(req.header("cookie"), Some(expected));
    }
    dir.assert_output("cookie.bin", &data).await;
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn compressed_full_responses_use_decoded_lengths_and_keep_206_strict() {
    use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder};
    use tokio::io::AsyncWrite;

    async fn encode(mut encoder: impl AsyncWrite + Unpin, data: &[u8]) {
        encoder.write_all(data).await.unwrap();
        encoder.shutdown().await.unwrap();
    }

    let data = b"compressed body\n".repeat(4096);
    let (mut gzip, mut deflate, mut brotli) = (Vec::new(), Vec::new(), Vec::new());
    encode(GzipEncoder::new(&mut gzip), &data).await;
    encode(ZlibEncoder::new(&mut deflate), &data).await;
    encode(BrotliEncoder::new(&mut brotli), &data).await;
    let server = Server::new(move |req| {
        let (coding, source) = match req.path.as_str() {
            "/gzip" | "/partial-gzip" => ("gzip", &gzip),
            "/deflate" => ("deflate", &deflate),
            "/br" => ("br", &brotli),
            _ => unreachable!(),
        };
        let source = Arc::new(source.clone());
        let mut plain = req.clone();
        if req.path != "/partial-gzip" {
            plain
                .headers
                .retain(|name, _| !name.eq_ignore_ascii_case("range"));
        }
        let mut reply = Reply::resource(&plain, &source, "\"encoded\"");
        reply.headers.push(("Content-Encoding", coding.into()));
        reply
    })
    .await;
    let dir = TestDir::new().await;
    let engine = DownloadEngine::new(dir.config()).await.unwrap();
    for coding in ["gzip", "deflate", "br"] {
        let id = engine
            .add_http_download(request(server.url(&format!("/{coding}")), None, 4))
            .await
            .unwrap();
        let done = completed(&engine, &id).await;
        assert_eq!(done.total_bytes, Some(data.len() as u64));
        assert!(!done.resumable);
        dir.assert_output(coding, &data).await;
    }
    let id = engine
        .add_http_download(request(server.url("/partial-gzip"), None, 1))
        .await
        .unwrap();
    assert!(failed(&engine, &id)
        .await
        .error_message
        .unwrap()
        .contains("unsupported Content-Encoding"));
    assert!(server
        .requests()
        .iter()
        .all(|req| req.header("accept-encoding") == Some("identity")));
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn compressed_members_require_clean_http_eof() {
    use crate::http::{ContentCoding, FetchProfile, ProbeMode};
    use async_compression::tokio::write::{BrotliEncoder, ZlibEncoder};
    use tokio::io::AsyncReadExt;

    let data = b"complete compressed member";
    let mut deflate = ZlibEncoder::new(Vec::new());
    deflate.write_all(data).await.unwrap();
    deflate.shutdown().await.unwrap();
    let mut brotli = BrotliEncoder::new(Vec::new());
    brotli.write_all(data).await.unwrap();
    brotli.shutdown().await.unwrap();
    let (deflate, brotli) = (deflate.into_inner(), brotli.into_inner());
    let server = Server::new(move |req| {
        let (coding, case) = req.path[1..].split_once('/').unwrap();
        let mut body = if coding == "br" {
            brotli.clone()
        } else {
            deflate.clone()
        };
        if case == "trailing" {
            body.extend_from_slice(b"trailing bytes");
        }
        let body = Arc::new(body);
        let mut reply = Reply::resource(req, &body, "\"encoded\"");
        reply.headers.push(("Content-Encoding", coding.into()));
        if case == "truncated" {
            reply.headers.retain(|(name, _)| *name != "Content-Length");
            reply
                .headers
                .push(("Content-Length", (body.len() + 1).to_string()));
        }
        reply
    })
    .await;
    let http = Http::new(true, Duration::from_secs(5), &EngineProxyConfig::Direct).unwrap();
    for coding in ["deflate", "br"] {
        for (case, kind) in [
            ("truncated", io::ErrorKind::UnexpectedEof),
            ("trailing", io::ErrorKind::InvalidData),
        ] {
            let url = server.url(&format!("/{coding}/{case}"));
            let source = Source::new(&url, &HashMap::new(), None, true).unwrap();
            let response = http
                .probe(&source, FetchProfile::default(), ProbeMode::Full)
                .await
                .unwrap()
                .response
                .unwrap();
            let mut reader = ContentCoding::from_response(&response)
                .unwrap()
                .reader(response);
            let mut decoded = Vec::new();
            let error = reader.read_to_end(&mut decoded).await.unwrap_err();
            assert_eq!(error.kind(), kind, "{coding}/{case}: {error}");
            assert_eq!(decoded, data, "{coding}/{case}");
        }
    }
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

#[tokio::test]
async fn pause_resume_and_remove_queue_persist_and_reassign_transfers() {
    let data = payload(512 * KIB, 19);
    let server = Server::new(move |req| {
        Reply::resource(req, &data, "\"queue-ops\"").with_delay(Duration::from_millis(12))
    })
    .await;
    let dir = TestDir::new().await;
    let config = dir.config();
    let engine = DownloadEngine::new(config.clone()).await.unwrap();
    engine
        .set_queue_config(QueueConfig {
            id: "night".into(),
            max_concurrent_downloads: 1,
            ..QueueConfig::default()
        })
        .await
        .unwrap();

    let mut req_a = request(server.url("/night-a.bin"), None, 1);
    req_a.queue_id = Some("night".into());
    let mut req_b = request(server.url("/night-b.bin"), None, 1);
    req_b.queue_id = Some("night".into());
    let a = engine.add_http_download(req_a).await.unwrap();
    wait_for(&engine, &a, |s| s.status == TransferStatus::Active).await;
    let b = engine.add_http_download(req_b).await.unwrap();
    assert_eq!(engine.snapshot(&b).unwrap().status, TransferStatus::Queued);
    assert!(engine
        .assign_queue_batch(&[b.clone(), a.clone()], DEFAULT_QUEUE)
        .is_err());
    assert!(engine
        .assign_queue_batch(&[b.clone(), "missing".into()], DEFAULT_QUEUE)
        .is_err());
    assert_eq!(engine.snapshot(&b).unwrap().queue_id, "night");

    engine.pause_queue("night").await.unwrap();
    wait_for(&engine, &a, |s| s.status == TransferStatus::Paused).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(engine.snapshot(&b).unwrap().status, TransferStatus::Queued);
    assert!(
        engine
            .queue_configs()
            .into_iter()
            .find(|q| q.id == "night")
            .unwrap()
            .paused
    );

    engine.resume_queue("night").await.unwrap();
    completed(&engine, &a).await;
    completed(&engine, &b).await;

    assert!(engine.remove_queue(DEFAULT_QUEUE).await.is_err());

    engine
        .set_queue_config(QueueConfig {
            id: "custom".into(),
            max_concurrent_downloads: 1,
            paused: true,
            ..QueueConfig::default()
        })
        .await
        .unwrap();
    let mut req_c = request(server.url("/custom.bin"), None, 1);
    req_c.queue_id = Some("custom".into());
    req_c.start_paused = true;
    let c = engine.add_http_download(req_c).await.unwrap();
    assert_eq!(engine.snapshot(&c).unwrap().queue_id, "custom");

    assert_eq!(
        engine
            .assign_queue_batch(&[c.clone(), c.clone()], "night")
            .unwrap(),
        1
    );
    assert_eq!(engine.snapshot(&c).unwrap().queue_id, "night");
    engine.assign_queue(&c, "custom").await.unwrap();
    engine.remove_queue("custom").await.unwrap();
    assert_eq!(engine.snapshot(&c).unwrap().queue_id, DEFAULT_QUEUE);
    assert!(!engine.queue_configs().iter().any(|q| q.id == "custom"));
    engine.shutdown().await.unwrap();

    let reopened = DownloadEngine::new(config).await.unwrap();
    assert_eq!(reopened.snapshot(&c).unwrap().queue_id, DEFAULT_QUEUE);
    assert!(!reopened.queue_configs().iter().any(|q| q.id == "custom"));
    reopened.shutdown().await.unwrap();
}
