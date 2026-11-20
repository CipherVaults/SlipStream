use super::Suite;
use crate::report::Outcome;
use slipstream_engine::{DownloadEngine, EngineConfig};
use slipstream_protocol::{AddHttpDownloadRequest, TransferSnapshot, TransferStatus};
use std::{
    collections::HashMap,
    fs,
    io::{self, ErrorKind},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
    time::{self, sleep},
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct LoggedRequest {
    path: String,
    headers: HashMap<String, String>,
}

#[derive(Clone)]
struct StaticReply {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl StaticReply {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            status: "200 OK".into(),
            headers: Vec::new(),
            body,
        }
    }

    fn redirect(status: &str, location: &str) -> Self {
        Self {
            status: status.into(),
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: "404 Not Found".into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

struct LoopbackServer {
    base_url: String,
    logs: Arc<Mutex<Vec<LoggedRequest>>>,
    task: JoinHandle<()>,
}

impl LoopbackServer {
    async fn start(responder: impl Fn(&str) -> StaticReply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind guard loopback listener");
        let addr = listener.local_addr().expect("guard listener addr");
        let logs = Arc::new(Mutex::new(Vec::new()));
        let logs_ref = Arc::clone(&logs);
        let responder = Arc::new(responder);
        let task = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break; };
                        let logs = Arc::clone(&logs_ref);
                        let responder = Arc::clone(&responder);
                        conns.spawn(async move {
                            let _ = handle_conn(&mut stream, &logs, &*responder).await;
                        });
                    }
                    _ = conns.join_next(), if !conns.is_empty() => {}
                }
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            logs,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn request_count(&self) -> usize {
        self.logs.lock().expect("guard log lock").len()
    }

    fn requests_since(&self, start: usize) -> Vec<LoggedRequest> {
        let guard = self.logs.lock().expect("guard log lock");
        guard.get(start..).unwrap_or_default().to_vec()
    }
}

fn count_files_strict(dir: &Path) -> Result<usize, String> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut count = 0usize;
            for entry in entries {
                entry.map_err(|e| format!("failed reading dir entry in {}: {e}", dir.display()))?;
                count += 1;
            }
            Ok(count)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(0),
        Err(err) => Err(format!("failed reading dir {}: {err}", dir.display())),
    }
}

#[cfg(windows)]
fn verify_motw_ads(path: &Path, expected_host_url: &str) -> Result<(), String> {
    let mut ads = path.as_os_str().to_os_string();
    ads.push(":Zone.Identifier");
    let zone = fs::read_to_string(&ads).map_err(|e| format!("read MotW ADS failed: {e}"))?;
    let expected_host = format!("HostUrl={expected_host_url}");
    let zone_lines: Vec<_> = zone.lines().collect();
    if zone_lines.first().copied() != Some("[ZoneTransfer]")
        || zone_lines.iter().filter(|&&l| l == "[ZoneTransfer]").count() != 1
        || zone_lines.iter().filter(|&&l| l == "ZoneId=3").count() != 1
        || zone_lines.iter().filter(|&&l| l == expected_host).count() != 1
        || zone_lines.iter().any(|l| l.starts_with("ReferrerUrl="))
    {
        return Err(format!("unexpected MotW ADS on {}: {zone:?}", path.display()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn verify_motw_ads(_path: &Path, _expected_host_url: &str) -> Result<(), String> {
    Ok(())
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_conn(
    stream: &mut TcpStream,
    logs: &Mutex<Vec<LoggedRequest>>,
    responder: &(impl Fn(&str) -> StaticReply + Send + Sync),
) -> io::Result<()> {
    let mut buf = [0u8; 4096];
    let mut len = 0;
    let header_text = loop {
        if len == buf.len() {
            return Ok(());
        }
        let n = stream.read(&mut buf[len..]).await?;
        if n == 0 {
            return Ok(());
        }
        let scan = len.saturating_sub(3);
        len += n;
        if let Some(pos) = buf[scan..len].windows(4).position(|w| w == b"\r\n\r\n") {
            break String::from_utf8_lossy(&buf[..scan + pos]).into_owned();
        }
    };
    let mut lines = header_text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_ascii_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(());
    };
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    logs.lock().unwrap().push(LoggedRequest {
        path: path.to_owned(),
        headers,
    });

    let reply = responder(path);
    let mut raw = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (k, v) in &reply.headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str("\r\n");
    if let Err(err) = stream.write_all(raw.as_bytes()).await {
        if matches!(
            err.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
        ) {
            return Ok(());
        }
        return Err(err);
    }
    if method != "HEAD" && !reply.body.is_empty() {
        let _ = stream.write_all(&reply.body).await;
    }
    let _ = stream.shutdown().await;
    Ok(())
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create guard temp dir");
        Self(path)
    }

    fn engine_config(&self, sub: &str) -> EngineConfig {
        let dir = self.0.join(sub);
        fs::create_dir_all(&dir).expect("create guard sub dir");
        EngineConfig {
            db_path: dir.join("state.sqlite"),
            default_download_dir: dir.join("downloads"),
            allow_loopback: true,
            request_timeout: Duration::from_secs(3),
            ..EngineConfig::default()
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn wait_terminal(engine: &DownloadEngine, id: &str) -> Result<TransferSnapshot, String> {
    time::timeout(WAIT, async {
        loop {
            let snap = engine.snapshot(id).map_err(|e| e.to_string())?;
            if matches!(
                snap.status,
                TransferStatus::Completed | TransferStatus::Failed | TransferStatus::Canceled
            ) {
                return Ok(snap);
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for DownloadEngine transfer".to_string())?
}

fn base_req(url: String) -> AddHttpDownloadRequest {
    AddHttpDownloadRequest {
        url,
        filename: None,
        save_dir: None,
        max_connections: Some(1),
        queue_id: None,
        headers: HashMap::new(),
        referrer: None,
        expected_sha256: None,
    }
}

pub(super) async fn run_content_disposition_cases(suite: &mut Suite) {
    let payload = b"slipstream-content-disposition-payload".to_vec();
    let payload_clone = payload.clone();
    let server = LoopbackServer::start(move |path| {
        let disposition = match path {
            "/bad-traversal" => "attachment; filename=\"../evil.bin\"".to_string(),
            "/bad-device" => "attachment; filename=\"CON.exe\"".to_string(),
            "/bad-ads" => "attachment; filename=\"evil.mp4:Zone.Identifier\"".to_string(),
            "/bad-bidi" => "attachment; filename=\"\u{202E}cod.exe\"".to_string(),
            "/bad-utf8-traversal" => {
                "attachment; filename*=UTF-8''..%2F..%2Fescape.exe".to_string()
            }
            "/bad-backslash" => "attachment; filename=\"sub\\\\evil.dll\"".to_string(),
            "/good-cafe" => {
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8'en'caf%C3%A9.bin"
                    .to_string()
            }
            "/good-semi" => "attachment; filename=\"a;b.zip\"".to_string(),
            "/good-tokyo" => "attachment; filename*=UTF-8''%E6%9D%B1%E4%BA%AC.mp4".to_string(),
            "/good-arabic" => "attachment; filename*=UTF-8''%D9%85%D9%84%D9%81.txt".to_string(),
            "/good-dots" => "attachment; filename=\"report.v1.2..final.pdf\"".to_string(),
            _ => "attachment; filename=\"fallback.bin\"".to_string(),
        };
        let mut reply = StaticReply::ok(payload_clone.clone());
        reply
            .headers
            .push(("Content-Disposition".into(), disposition));
        reply
    })
    .await;

    let temp = TempDir::new("slipstream-eval-cd");
    let config = temp.engine_config("cd");
    let save_dir = config.default_download_dir.clone();
    let engine = DownloadEngine::new(config).await.expect("open cd engine");

    for (index, (path, expected_sub)) in [
        ("/bad-traversal", "forbidden directory separator"),
        ("/bad-device", "reserved device name"),
        ("/bad-ads", "alternate data stream"),
        ("/bad-bidi", "bidi"),
        ("/bad-utf8-traversal", "forbidden directory separator"),
        ("/bad-backslash", "forbidden directory separator"),
    ]
    .into_iter()
    .enumerate()
    {
        let item = suite.register(
            "filesystem_path",
            "harmful",
            "content_disposition_engine",
            "composed",
            format!("cd-harmful:{path}").into_bytes(),
        );
        let sub_dir = save_dir.join(format!("bad-{index}"));
        let outcome = match engine
            .add_http_download(AddHttpDownloadRequest {
                save_dir: Some(sub_dir.to_string_lossy().into_owned()),
                ..base_req(server.url(path))
            })
            .await
        {
            Ok(id) => match wait_terminal(&engine, &id).await {
                Ok(snap) => match count_files_strict(&sub_dir) {
                    Ok(count) => {
                        let msg = snap.error_message.unwrap_or_default().to_ascii_lowercase();
                        if snap.status == TransferStatus::Failed
                            && msg.contains(expected_sub)
                            && count == 0
                        {
                            Outcome::Pass("DownloadEngine rejected unsafe Content-Disposition and wrote zero files.")
                        } else {
                            Outcome::Fail(format!(
                                "unsafe Content-Disposition was not blocked cleanly: status={:?}, msg={msg:?}, files={count}",
                                snap.status
                            ))
                        }
                    }
                    Err(err) => Outcome::Broken(err),
                },
                Err(err) => Outcome::Broken(err),
            },
            Err(err) => Outcome::Broken(format!("add_http_download failed unexpectedly: {err}")),
        };
        suite.records.push(item.record(outcome, 1, None));
    }

    for (index, (path, expected_name)) in [
        ("/good-cafe", "café.bin"),
        ("/good-semi", "a;b.zip"),
        ("/good-tokyo", "東京.mp4"),
        ("/good-arabic", "ملف.txt"),
        ("/good-dots", "report.v1.2..final.pdf"),
    ]
    .into_iter()
    .enumerate()
    {
        let item = suite.register(
            "filesystem_path",
            "benign",
            "content_disposition_engine",
            "composed",
            format!("cd-benign:{path}:{expected_name}").into_bytes(),
        );
        let sub_dir = save_dir.join(format!("good-{index}"));
        let req_url = server.url(path);
        let outcome = match engine
            .add_http_download(AddHttpDownloadRequest {
                save_dir: Some(sub_dir.to_string_lossy().into_owned()),
                ..base_req(req_url.clone())
            })
            .await
        {
            Ok(id) => match wait_terminal(&engine, &id).await {
                Ok(snap) => {
                    let out_path = sub_dir.join(expected_name);
                    let bytes_ok = fs::read(&out_path).ok().as_deref() == Some(payload.as_slice());
                    let motw_res = verify_motw_ads(&out_path, &req_url);
                    if snap.status == TransferStatus::Completed
                        && snap.filename == expected_name
                        && bytes_ok
                        && motw_res.is_ok()
                    {
                        Outcome::Pass("DownloadEngine sanitized and committed the UTF-8 Content-Disposition filename with MotW.")
                    } else {
                        Outcome::Fail(format!(
                            "benign Content-Disposition failed: status={:?}, filename={:?}, bytes_ok={bytes_ok}, motw={:?}",
                            snap.status, snap.filename, motw_res
                        ))
                    }
                }
                Err(err) => Outcome::Broken(err),
            },
            Err(err) => Outcome::Broken(format!("add_http_download failed: {err}")),
        };
        suite.records.push(item.record(outcome, 1, None));
    }

    let _ = engine.shutdown().await;
}

pub(super) async fn run_http_redirect_cases(suite: &mut Suite) {
    let payload = b"slipstream-redirect-engine-payload".to_vec();
    let payload_final = payload.clone();
    let payload_rel = payload.clone();

    let origin_url_slot = Arc::new(Mutex::new(String::new()));
    let origin_url_for_target = Arc::clone(&origin_url_slot);
    let target_server = LoopbackServer::start(move |path| match path {
        "/bounce" => StaticReply::redirect("302 Found", &origin_url_for_target.lock().unwrap()),
        "/referer-landing" => StaticReply::ok(payload_rel.clone()),
        _ => StaticReply::redirect("302 Found", "http://10.0.0.1/blocked"),
    })
    .await;

    let target_bounce = target_server.url("/bounce");
    let target_referer = target_server.url("/referer-landing");
    let origin_server = LoopbackServer::start(move |path| {
        if let Some(hop_str) = path.strip_prefix("/hop/") {
            if let Ok(hop) = hop_str.parse::<usize>() {
                if hop > 0 {
                    return StaticReply::redirect("302 Found", &format!("/hop/{}", hop - 1));
                }
                return StaticReply::ok(payload_final.clone());
            }
        }
        match path {
            "/ssrf-hop1" => StaticReply::redirect("302 Found", "/ssrf-hop2"),
            "/ssrf-hop2" => StaticReply::redirect("302 Found", "http://10.0.0.1/private"),
            "/ssrf-link-local" => StaticReply::redirect(
                "301 Moved Permanently",
                "http://169.254.169.254/latest/meta-data/",
            ),
            "/ssrf-172" => {
                StaticReply::redirect("307 Temporary Redirect", "http://172.16.0.1/admin")
            }
            "/ssrf-192" => {
                StaticReply::redirect("308 Permanent Redirect", "http://192.168.1.1/router")
            }
            "/ssrf-cgnat" => StaticReply::redirect("303 See Other", "http://100.64.0.1/cgnat"),
            "/ssrf-v4mapped" => {
                StaticReply::redirect("302 Found", "http://[::ffff:127.0.0.1]/admin")
            }
            "/ssrf-file" => StaticReply::redirect("302 Found", "file:///C:/Windows/win.ini"),
            "/cross-start" => StaticReply::redirect("302 Found", &target_bounce),
            "/ref-start" => StaticReply::redirect("307 Temporary Redirect", &target_referer),
            "/rel-start" => StaticReply::redirect("302 Found", "../assets/step2"),
            "/assets/step2" => StaticReply::redirect("301 Moved Permanently", "/final-ok"),
            "/final-ok" => StaticReply::ok(payload_final.clone()),
            _ => StaticReply::not_found(),
        }
    })
    .await;
    *origin_url_slot.lock().unwrap() = origin_server.url("/final-ok");

    let temp = TempDir::new("slipstream-eval-redir");
    let config = temp.engine_config("redir");
    let save_dir = config.default_download_dir.clone();
    let engine = DownloadEngine::new(config)
        .await
        .expect("open redir engine");

    // 8 harmful multi-hop 30x HTTP redirect SSRF/scheme/loop-cap cases through real DownloadEngine
    for (surface, path, expected_sub) in [
        ("engine_302_multihop_rfc1918", "/ssrf-hop1", "10.0.0.1"),
        (
            "engine_301_link_local",
            "/ssrf-link-local",
            "169.254.169.254",
        ),
        ("engine_307_rfc1918_172", "/ssrf-172", "172.16.0.1"),
        ("engine_308_rfc1918_192", "/ssrf-192", "192.168.1.1"),
        ("engine_303_cgnat", "/ssrf-cgnat", "100.64.0.1"),
        ("engine_302_ipv4_mapped_v6", "/ssrf-v4mapped", "blocked"),
        ("engine_302_disallowed_scheme", "/ssrf-file", "scheme"),
        (
            "engine_302_redirect_hop_limit",
            "/hop/11",
            "too many redirects",
        ),
    ] {
        let item = suite.register(
            "url_ssrf_redirect",
            "harmful",
            surface,
            "composed",
            format!("engine-harmful:{surface}:{path}").into_bytes(),
        );
        let sub = save_dir.join(surface);
        let outcome = match engine
            .add_http_download(AddHttpDownloadRequest {
                filename: Some("blocked.bin".into()),
                save_dir: Some(sub.to_string_lossy().into_owned()),
                ..base_req(origin_server.url(path))
            })
            .await
        {
            Ok(id) => match wait_terminal(&engine, &id).await {
                Ok(snap) => match count_files_strict(&sub) {
                    Ok(count) => {
                        let msg = snap.error_message.unwrap_or_default().to_ascii_lowercase();
                        if snap.status == TransferStatus::Failed
                            && msg.contains(expected_sub)
                            && count == 0
                        {
                            Outcome::Pass("DownloadEngine blocked the multi-hop redirect target with zero output files.")
                        } else {
                            Outcome::Fail(format!(
                                "engine redirect check failed: status={:?}, msg={msg:?}, files={count}",
                                snap.status
                            ))
                        }
                    }
                    Err(err) => Outcome::Broken(err),
                },
                Err(err) => Outcome::Broken(err),
            },
            Err(err) => Outcome::Broken(format!("add_http_download failed: {err}")),
        };
        suite.records.push(item.record(outcome, 1, None));
    }

    // 4 benign real DownloadEngine cross-origin credential-stripping & multi-hop checks
    // Note: HTTPS->HTTP Referer downgrade stripping is exercised in crates/engine's unit test
    // on apply_redirect_header_policy because loopback ChaosServer is plain HTTP; here we verify
    // end-to-end cross-origin Cookie/Cookie2/Authorization/Proxy-Authorization permanent stripping,
    // Referer userinfo/fragment sanitization across redirects, 10-hop cap boundary, and relative 30x.
    let cross_item = suite.register(
        "url_ssrf_redirect",
        "benign",
        "engine_cross_origin_cookie_strip",
        "composed",
        b"engine-benign:cross-origin-cookie-strip".to_vec(),
    );
    let sensitive = ["cookie", "cookie2", "authorization", "proxy-authorization"];
    let cross_dir = save_dir.join("cross-strip");
    let mut cross_req = base_req(origin_server.url("/cross-start"));
    cross_req.filename = Some("cross.bin".into());
    cross_req.save_dir = Some(cross_dir.to_string_lossy().into_owned());
    cross_req.headers = sensitive
        .into_iter()
        .map(|k| (k.to_owned(), "secret-token".to_owned()))
        .collect();
    let origin_mark = origin_server.request_count();
    let target_mark = target_server.request_count();
    let cross_outcome = match engine.add_http_download(cross_req).await {
        Ok(id) => match wait_terminal(&engine, &id).await {
            Ok(snap) => {
                let target_reqs: Vec<_> = target_server
                    .requests_since(target_mark)
                    .into_iter()
                    .filter(|r| r.path == "/bounce")
                    .collect();
                let origin_reqs = origin_server.requests_since(origin_mark);
                let final_reqs: Vec<_> =
                    origin_reqs.iter().filter(|r| r.path == "/final-ok").collect();
                let initial_reqs: Vec<_> = origin_reqs
                    .iter()
                    .filter(|r| r.path == "/cross-start")
                    .collect();
                let clean_target = !target_reqs.is_empty()
                    && target_reqs
                        .iter()
                        .all(|r| sensitive.iter().all(|h| !r.headers.contains_key(*h)));
                let clean_return = !final_reqs.is_empty()
                    && final_reqs
                        .iter()
                        .all(|r| sensitive.iter().all(|h| !r.headers.contains_key(*h)));
                let saw_initial = !initial_reqs.is_empty()
                    && initial_reqs.iter().all(|r| {
                        sensitive.iter().all(|h| {
                            r.headers.get(*h).map(String::as_str) == Some("secret-token")
                        })
                    });
                let bytes_ok = fs::read(cross_dir.join("cross.bin")).ok().as_deref()
                    == Some(payload.as_slice());
                if snap.status == TransferStatus::Completed
                    && clean_target
                    && clean_return
                    && saw_initial
                    && bytes_ok
                {
                    Outcome::Pass("DownloadEngine permanently stripped Cookie/Auth credentials on cross-origin redirect.")
                } else {
                    Outcome::Fail(format!(
                        "cross-origin strip check failed: status={:?}, clean_target={clean_target}, clean_return={clean_return}, saw_initial={saw_initial}, bytes_ok={bytes_ok}",
                        snap.status
                    ))
                }
            }
            Err(err) => Outcome::Broken(err),
        },
        Err(err) => Outcome::Broken(format!("add_http_download failed: {err}")),
    };
    suite
        .records
        .push(cross_item.record(cross_outcome, 1, None));

    let ref_item = suite.register(
        "url_ssrf_redirect",
        "benign",
        "engine_redirect_referer_sanitize",
        "composed",
        b"engine-benign:redirect-referer-sanitize".to_vec(),
    );
    let ref_dir = save_dir.join("ref-sanitize");
    let mut ref_req = base_req(origin_server.url("/ref-start"));
    ref_req.filename = Some("ref.bin".into());
    ref_req.save_dir = Some(ref_dir.to_string_lossy().into_owned());
    ref_req.referrer = Some("https://alice:pw@ref.example.test/watch?v=1#frag".into());
    ref_req.headers = [("Cookie".into(), "sid=keep-off-target".into())].into();
    let target_mark = target_server.request_count();
    let ref_outcome = match engine.add_http_download(ref_req).await {
        Ok(id) => match wait_terminal(&engine, &id).await {
            Ok(snap) => {
                let target_reqs = target_server.requests_since(target_mark);
                let landing: Vec<_> = target_reqs
                    .iter()
                    .filter(|r| r.path == "/referer-landing")
                    .collect();
                let ok = snap.status == TransferStatus::Completed
                    && !landing.is_empty()
                    && landing.iter().all(|r| {
                        !r.headers.contains_key("cookie")
                            && r.headers.get("referer").map(String::as_str)
                                == Some("https://ref.example.test/watch?v=1")
                    })
                    && fs::read(ref_dir.join("ref.bin")).ok().as_deref()
                        == Some(payload.as_slice());
                if ok {
                    Outcome::Pass("DownloadEngine sanitized Referer credentials/fragment and stripped Cookie cross-origin.")
                } else {
                    Outcome::Fail(
                        "Referer sanitization or cross-origin Cookie strip failed on redirect"
                            .into(),
                    )
                }
            }
            Err(err) => Outcome::Broken(err),
        },
        Err(err) => Outcome::Broken(format!("add_http_download failed: {err}")),
    };
    suite.records.push(ref_item.record(ref_outcome, 1, None));

    for (surface, path, out_name) in [
        ("engine_302_10hop_boundary", "/hop/10", "ten_hops.bin"),
        ("engine_302_relative_chain", "/rel-start", "rel_chain.bin"),
    ] {
        let item = suite.register(
            "url_ssrf_redirect",
            "benign",
            surface,
            "boundary",
            format!("engine-benign:{surface}:{path}").into_bytes(),
        );
        let sub = save_dir.join(surface);
        let origin_mark = origin_server.request_count();
        let outcome = match engine
            .add_http_download(AddHttpDownloadRequest {
                filename: Some(out_name.into()),
                save_dir: Some(sub.to_string_lossy().into_owned()),
                ..base_req(origin_server.url(path))
            })
            .await
        {
            Ok(id) => match wait_terminal(&engine, &id).await {
                Ok(snap) => {
                    let reqs = origin_server.requests_since(origin_mark);
                    let traversed_ok = match surface {
                        "engine_302_10hop_boundary" => {
                            (0..=10)
                                .rev()
                                .all(|h| reqs.iter().any(|r| r.path == format!("/hop/{h}")))
                                && reqs.iter().any(|r| r.path == "/final-ok")
                        }
                        "engine_302_relative_chain" => ["/rel-start", "/assets/step2", "/final-ok"]
                            .iter()
                            .all(|p| reqs.iter().any(|r| r.path == *p)),
                        _ => false,
                    };
                    let bytes_ok =
                        fs::read(sub.join(out_name)).ok().as_deref() == Some(payload.as_slice());
                    if snap.status == TransferStatus::Completed && traversed_ok && bytes_ok {
                        Outcome::Pass("DownloadEngine completed the valid redirect chain cleanly.")
                    } else {
                        Outcome::Fail(format!(
                            "valid redirect chain failed: status={:?}, traversed_ok={traversed_ok}, bytes_ok={bytes_ok}",
                            snap.status
                        ))
                    }
                }
                Err(err) => Outcome::Broken(err),
            },
            Err(err) => Outcome::Broken(format!("add_http_download failed: {err}")),
        };
        suite.records.push(item.record(outcome, 1, None));
    }

    let _ = engine.shutdown().await;
}
