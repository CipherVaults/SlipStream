use base64::{engine::general_purpose::STANDARD, Engine as _};
use minisign_verify::{PublicKey, Signature};
use reqwest::{redirect::Policy, Client, RequestBuilder, Response, StatusCode};
use semver::Version;
use serde::Deserialize;
use slipstream_protocol::UpdateStatus;
use std::{borrow::Cow, fmt::Display, time::Duration};
use url::Url;

#[cfg(windows)]
use {
    crate::AppState,
    slipstream_protocol::{UpdateProgress, UpdateProgressStage},
    std::{
        fs::{self, OpenOptions},
        io::Write,
        path::{Path, PathBuf},
        process::Command,
        sync::Arc,
        time::Instant,
    },
    tokio::sync::Mutex,
    uuid::Uuid,
};

pub const DEFAULT_UPDATER_PUBKEY_B64: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDI3NEQyMDc4MTczMTcxNzkKUldSNWNURVhlQ0JOSjZ6anFKYk5pbVNySFJ0SE1aVkRjYmpIR2JwaURFcFVqNDl3dDNja0dOc3UK";
pub const DEFAULT_UPDATER_ENDPOINT: &str =
    "https://github.com/CipherVaults/SlipStream/releases/latest/download/latest.json";

const MANIFEST_LIMIT: usize = 512 * 1024;
const CHECK_ACTION: &str = "check for updates";

#[cfg(windows)]
const INSTALLER_LIMIT: usize = 512 * 1024 * 1024;
#[cfg(windows)]
const INSTALLER_TIMEOUT: Duration = Duration::from_secs(600);
#[cfg(windows)]
static INSTALL: Mutex<()> = Mutex::const_new(());

#[derive(Deserialize)]
struct LatestReleaseManifest {
    version: String,
    notes: Option<String>,
    pub_date: Option<String>,
    platforms: ReleasePlatforms,
}

#[derive(Deserialize)]
struct ReleasePlatforms {
    #[serde(rename = "windows-x86_64")]
    windows_x86_64: ReleaseAsset,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    signature: String,
    url: String,
}

struct AvailableRelease {
    version: String,
    notes: Option<String>,
    pub_date: Option<String>,
    #[cfg_attr(not(windows), allow(dead_code))]
    signature: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    url: Url,
}

pub fn friendly_updater_error(action: &str, error: impl Display) -> String {
    let error = error.to_string().to_ascii_lowercase();
    let reason = if ["404", "release not found", "release json from the remote"]
        .iter()
        .any(|text| error.contains(text))
    {
        "The update release could not be found. Try again later."
    } else if [
        "signature",
        "pubkey",
        "public key",
        "verification",
        "minisign",
        "base64",
    ]
    .iter()
    .any(|text| error.contains(text))
    {
        "The update signature could not be verified."
    } else if ["json", "parse", "at line", "decode response body"]
        .iter()
        .any(|text| error.contains(text))
    {
        "The update server returned invalid release information. Try again later."
    } else if [
        "network",
        "connection",
        "connect",
        "sending request",
        "timeout",
        "timed out",
        "download request failed",
        "request or response body",
        "dns",
    ]
    .iter()
    .any(|text| error.contains(text))
    {
        "Could not reach the update server. Check your connection and try again."
    } else {
        "Try again later."
    };
    format!("Could not {action}. {reason}")
}

fn allow_update_url(url: &Url) -> bool {
    if !url.has_host() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    #[cfg(test)]
    if url.scheme() == "http" && url.host_str() == Some("127.0.0.1") {
        return true;
    }
    url.scheme() == "https"
}

fn update_url(value: &str) -> Result<Url, &'static str> {
    let url = Url::parse(value).map_err(|_| "invalid release URL")?;
    if !allow_update_url(&url) {
        return Err("release URL must use HTTPS without credentials");
    }
    Ok(url)
}

fn client() -> Result<Client, String> {
    Client::builder()
        .use_rustls_tls()
        .no_proxy()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(20))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() > 10 || !allow_update_url(attempt.url()) {
                attempt.error("Rejected update redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| "could not initialize the update network client".into())
}

async fn get(request: RequestBuilder) -> Result<Response, String> {
    let response = request
        .send()
        .await
        .map_err(|_| "update network request failed")?;
    match response.status() {
        StatusCode::NOT_FOUND => Err("release not found".into()),
        status if status.is_success() => Ok(response),
        status => Err(format!("update network request returned HTTP {status}")),
    }
}

async fn read_response(
    mut response: Response,
    limit: usize,
    mut on_chunk: impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, &'static str> {
    let total = response.content_length();
    if total.is_some_and(|length| length > limit as u64) {
        return Err("response exceeds the size limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "network response failed")?
    {
        let len = bytes.len() + chunk.len();
        if len > limit {
            return Err("response exceeds the size limit");
        }
        if len > bytes.capacity() {
            let capacity = (bytes.capacity() * 2).max(len).min(limit);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .map_err(|_| "could not allocate memory for the update response")?;
        }
        bytes.extend_from_slice(&chunk);
        on_chunk(len as u64, total);
    }
    Ok(bytes)
}

async fn check_release(
    client: &Client,
    endpoint: &str,
    pubkey_b64: &str,
    current_version: &str,
) -> Result<Option<AvailableRelease>, String> {
    public_key(pubkey_b64)?;
    let current = Version::parse(current_version).map_err(|_| "invalid current version")?;
    let endpoint = update_url(endpoint)?;
    let response = get(client.get(endpoint)).await?;
    let bytes = read_response(response, MANIFEST_LIMIT, |_, _| {})
        .await
        .map_err(|error| {
            if error == "network response failed" {
                error.to_owned()
            } else {
                format!("invalid release JSON response: {error}")
            }
        })?;
    let manifest: LatestReleaseManifest =
        serde_json::from_slice(&bytes).map_err(|_| "invalid release JSON")?;
    let remote = Version::parse(&manifest.version).map_err(|_| "invalid release JSON version")?;
    let asset = manifest.platforms.windows_x86_64;
    let url = update_url(&asset.url).map_err(|error| format!("invalid release JSON: {error}"))?;
    Ok((remote > current).then_some(AvailableRelease {
        version: manifest.version,
        notes: manifest.notes,
        pub_date: manifest.pub_date,
        signature: asset.signature,
        url,
    }))
}

#[cfg(windows)]
pub async fn check_for_updates(
    current_version: &str,
    startup: bool,
    state: &AppState,
) -> Result<Option<UpdateStatus>, String> {
    if startup && !state.claim_startup_update_check() {
        return Ok(None);
    }
    check_for_updates_from_url(
        DEFAULT_UPDATER_ENDPOINT,
        DEFAULT_UPDATER_PUBKEY_B64,
        current_version,
    )
    .await
    .map(Some)
}

pub async fn check_for_updates_from_url(
    endpoint: &str,
    pubkey_b64: &str,
    current_version: &str,
) -> Result<UpdateStatus, String> {
    let client = client().map_err(|error| friendly_updater_error(CHECK_ACTION, error))?;
    let release = check_release(&client, endpoint, pubkey_b64, current_version)
        .await
        .map_err(|error| friendly_updater_error(CHECK_ACTION, error))?;
    let mut status = UpdateStatus {
        available: release.is_some(),
        current_version: current_version.into(),
        version: None,
        notes: None,
        pub_date: None,
    };
    if let Some(release) = release {
        status.version = Some(release.version);
        status.notes = release.notes;
        status.pub_date = release.pub_date;
    }
    Ok(status)
}

fn minisign_text(input: &str) -> Result<Cow<'_, str>, String> {
    let input = input.trim();
    if input.starts_with("untrusted comment:") {
        return Ok(Cow::Borrowed(input));
    }
    let bytes = STANDARD
        .decode(input)
        .map_err(|error| format!("Invalid Minisign base64: {error}"))?;
    String::from_utf8(bytes)
        .map(Cow::Owned)
        .map_err(|_| "Minisign text is not UTF-8".into())
}

fn public_key(input: &str) -> Result<PublicKey, String> {
    let input = input.trim();
    let key = if input.starts_with("RW") || input.starts_with("RU") {
        Cow::Borrowed(input)
    } else {
        let text = minisign_text(input)?;
        Cow::Owned(
            text.lines()
                .nth(1)
                .ok_or("Missing Minisign public key line")?
                .trim()
                .to_owned(),
        )
    };
    PublicKey::from_base64(&key).map_err(|error| format!("Invalid Minisign public key: {error}"))
}

pub fn verify_release_signature(
    pubkey_b64_or_text: &str,
    sig_b64_or_text: &str,
    installer_bytes: &[u8],
    expected_version: &str,
) -> Result<(), String> {
    let public_key = public_key(pubkey_b64_or_text)?;
    let text = minisign_text(sig_b64_or_text)?;
    let signature = Signature::decode(text.trim())
        .map_err(|error| format!("Invalid Minisign signature: {error}"))?;
    public_key
        .verify(installer_bytes, &signature, false)
        .map_err(|error| format!("Minisign signature verification failed: {error}"))?;

    let mut versions = signature
        .trusted_comment()
        .split('\t')
        .filter_map(|token| token.strip_prefix("version:"));
    match versions.next() {
        Some(version) if version == expected_version && versions.next().is_none() => Ok(()),
        Some(_) => Err("The Minisign signature version does not match the release".into()),
        None => Err("The Minisign signature is missing the release version".into()),
    }
}

#[cfg(windows)]
pub async fn install_update<F>(
    state: Arc<AppState>,
    current_version: &str,
    mut on_progress: F,
) -> Result<bool, String>
where
    F: FnMut(UpdateProgress),
{
    let _install = INSTALL
        .try_lock()
        .map_err(|_| "An update is already in progress.")?;
    let client = client().map_err(|error| friendly_updater_error(CHECK_ACTION, error))?;
    let Some(release) = check_release(
        &client,
        DEFAULT_UPDATER_ENDPOINT,
        DEFAULT_UPDATER_PUBKEY_B64,
        current_version,
    )
    .await
    .map_err(|error| friendly_updater_error(CHECK_ACTION, error))?
    else {
        return Ok(false);
    };

    let mut emit = |stage, downloaded_bytes, total_bytes| {
        on_progress(UpdateProgress {
            stage,
            downloaded_bytes,
            total_bytes,
        });
    };
    emit(UpdateProgressStage::Downloading, 0, None);
    let response = get(client.get(release.url).timeout(INSTALLER_TIMEOUT))
        .await
        .map_err(|error| friendly_updater_error("download the update", error))?;
    let total = response.content_length();
    let mut last_emit = Instant::now();
    let bytes = read_response(response, INSTALLER_LIMIT, |downloaded, total| {
        if last_emit.elapsed() >= Duration::from_millis(80) || total == Some(downloaded) {
            last_emit = Instant::now();
            emit(UpdateProgressStage::Downloading, downloaded, total);
        }
    })
    .await
    .map_err(|error| friendly_updater_error("download the update", error))?;

    let downloaded = bytes.len() as u64;
    emit(UpdateProgressStage::Verifying, downloaded, total);
    let updates_dir = state.data_dir().join("updates");
    let expected_version = release.version.clone();
    let expected_sig = release.signature.clone();
    let mut installer = tokio::task::spawn_blocking(move || {
        let mut staged = StagedInstaller::stage_and_verify(
            &updates_dir,
            &bytes,
            &expected_sig,
            &expected_version,
        )?;
        Ok::<_, String>(staged.take_guard())
    })
    .await
    .map_err(|error| friendly_updater_error("prepare the update installer", error))??;

    emit(UpdateProgressStage::Installing, downloaded, total);
    const RECOVERY: &str = "Quit and reopen SlipStream to resume downloads.";
    state.shutdown().await.map_err(|error| {
        tracing::error!(%error, "could not stop the daemon for the update");
        format!("Could not stop downloads to prepare the update. {RECOVERY}")
    })?;
    // Hold the read-only (FILE_SHARE_READ) lock across spawn so no other process can modify the verified binary.
    Command::new(&installer.path)
        .args(["/S", "/UPDATE", "/R"])
        .spawn()
        .map_err(|error| {
            tracing::error!(%error, "could not launch the update installer");
            format!("Could not install the update. {RECOVERY}")
        })?;
    installer.remove_on_drop = false;
    Ok(true)
}

#[cfg(windows)]
pub fn cleanup_stale_update_dirs(data_dir: &Path) {
    let updates_dir = data_dir.join("updates");
    let Ok(entries) = fs::read_dir(&updates_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let _ = fs::remove_dir_all(entry.path());
    }
}

#[cfg(windows)]
struct StagedInstaller {
    dir: PathBuf,
    path: PathBuf,
    _lock: Option<fs::File>,
    remove_on_drop: bool,
}

#[cfg(windows)]
impl StagedInstaller {
    fn stage_and_verify(
        updates_dir: &Path,
        bytes: &[u8],
        sig: &str,
        expected_version: &str,
    ) -> Result<Self, String> {
        use std::{io::Read, os::windows::fs::OpenOptionsExt};
        const FILE_SHARE_READ: u32 = 1;

        crate::paths::ensure_safe_directory(updates_dir)
            .map_err(|e| friendly_updater_error("save the update installer", e))?;
        let dir = updates_dir.join(format!("update-{}", Uuid::new_v4()));
        fs::create_dir(&dir).map_err(|e| friendly_updater_error("save the update installer", e))?;
        let path = dir.join("SlipStream-update.exe");
        let guard = Self {
            dir: dir.clone(),
            path: path.clone(),
            _lock: None,
            remove_on_drop: true,
        };
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| friendly_updater_error("save the update installer", e))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| friendly_updater_error("save the update installer", e))?;
        drop(file);

        // Open read-only denying write and delete sharing (FILE_SHARE_READ only), then verify bytes read through this handle.
        let mut locked = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .map_err(|e| friendly_updater_error("verify the update", e))?;
        let mut disk_bytes = Vec::with_capacity(bytes.len());
        locked
            .read_to_end(&mut disk_bytes)
            .map_err(|e| friendly_updater_error("verify the update", e))?;
        verify_release_signature(
            DEFAULT_UPDATER_PUBKEY_B64,
            sig,
            &disk_bytes,
            expected_version,
        )
        .map_err(|error| friendly_updater_error("verify the update", error))?;

        let mut guard = guard;
        guard._lock = Some(locked);
        Ok(guard)
    }

    fn take_guard(&mut self) -> Self {
        let taken = Self {
            dir: self.dir.clone(),
            path: self.path.clone(),
            _lock: self._lock.take(),
            remove_on_drop: self.remove_on_drop,
        };
        self.remove_on_drop = false;
        taken
    }
}

#[cfg(windows)]
impl Drop for StagedInstaller {
    fn drop(&mut self) {
        self._lock.take();
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;
    use serde_json::json;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        task::JoinHandle,
    };

    const INSTALLER: &[u8] = b"SlipStream installer test bytes";
    const SIGNED_COMMENT: &str = "timestamp:1700000000\tfile:SlipStream-setup.exe\tversion:2.0.0";

    fn sign(keys: &KeyPair, comment: &str) -> String {
        minisign::sign(None, &keys.sk, INSTALLER, Some(comment), None)
            .unwrap()
            .to_string()
    }

    #[test]
    fn verifies_tauri_wrapped_and_plain_prehashed_signatures() {
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let pubkey = String::from_utf8(keys.pk.to_box().unwrap().to_bytes()).unwrap();
        let signature = sign(&keys, SIGNED_COMMENT);
        assert!(signature.lines().nth(1).unwrap().starts_with("RU"));

        let wrapped_pubkey = STANDARD.encode(&pubkey);
        let wrapped_signature = format!("{}\n", STANDARD.encode(&signature));
        let raw_key = keys.pk.to_base64();
        for pubkey in [&wrapped_pubkey, &pubkey, &raw_key] {
            for signature in [&wrapped_signature, &signature] {
                verify_release_signature(pubkey, signature, INSTALLER, "2.0.0").unwrap();
            }
        }
        public_key(DEFAULT_UPDATER_PUBKEY_B64).unwrap();
    }

    #[test]
    fn rejects_tampering_and_bad_minisign_encoding() {
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let pubkey = keys.pk.to_base64();
        let signature = sign(&keys, SIGNED_COMMENT);

        let mut tampered = INSTALLER.to_vec();
        tampered[0] ^= 1;
        assert!(verify_release_signature(&pubkey, &signature, &tampered, "2.0.0").is_err());
        assert!(verify_release_signature(&pubkey, &signature, INSTALLER, "2.0.1").is_err());
        assert!(verify_release_signature("%bad", &signature, INSTALLER, "2.0.0").is_err());
        assert!(verify_release_signature(&pubkey, "%bad", INSTALLER, "2.0.0").is_err());

        let edited_comment = signature.replace("version:2.0.0", "version:2.0.1");
        let error =
            verify_release_signature(&pubkey, &edited_comment, INSTALLER, "2.0.1").unwrap_err();
        assert!(error.contains("verification failed"));
    }

    #[test]
    fn requires_exactly_one_matching_tab_delimited_version_token() {
        let keys = KeyPair::generate_unencrypted_keypair().unwrap();
        let pubkey = keys.pk.to_base64();
        for comment in [
            "timestamp:1700000000\tfile:SlipStream-setup.exe",
            "timestamp:1700000000\tfile:version:2.0.0",
            "timestamp:1700000000 version:2.0.0",
            "timestamp:1700000000\tversion:2.0.0-rc.1",
            "timestamp:1700000000\tversion:2.0.0\tversion:2.0.1",
        ] {
            let signature = sign(&keys, comment);
            let error =
                verify_release_signature(&pubkey, &signature, INSTALLER, "2.0.0").unwrap_err();
            assert!(error.contains("version"), "{error}");
        }
    }

    #[test]
    fn keeps_friendly_updater_error_categories() {
        for (internal, expected) in [
            (
                "HTTP 404",
                "The update release could not be found. Try again later.",
            ),
            (
                "Invalid Minisign public key",
                "The update signature could not be verified.",
            ),
            (
                "invalid release JSON",
                "The update server returned invalid release information. Try again later.",
            ),
            (
                "network response failed",
                "Could not reach the update server. Check your connection and try again.",
            ),
            ("installer launch failed", "Try again later."),
        ] {
            assert_eq!(
                friendly_updater_error(CHECK_ACTION, internal),
                format!("Could not check for updates. {expected}")
            );
        }
    }

    fn manifest(version: &str, asset_url: &str) -> String {
        json!({
            "version": version,
            "notes": "Update notes",
            "pub_date": "2026-11-27T00:00:00Z",
            "platforms": {
                "windows-x86_64": {
                    "signature": "checked at install time",
                    "url": asset_url,
                }
            }
        })
        .to_string()
    }

    async fn serve_once(response: String) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}/latest.json", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, "GET /latest.json HTTP/1.1\r\n");
            loop {
                line.clear();
                if stream.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            stream
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });
        (url, task)
    }

    async fn mocked_check(response: String, current: &str) -> Result<UpdateStatus, String> {
        let (url, server) = serve_once(response).await;
        let result = check_for_updates_from_url(&url, DEFAULT_UPDATER_PUBKEY_B64, current).await;
        server.await.unwrap();
        result
    }

    fn ok_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn checks_release_metadata_and_compares_semver_over_http() {
        let asset = "https://example.com/SlipStream-setup.exe";
        for (remote, current, available) in [
            ("2.10.0", "2.9.0", true),
            ("2.0.0", "2.0.0-rc.1", true),
            ("2.0.0-rc.1", "2.0.0", false),
            ("2.0.0", "2.0.0", false),
            ("1.9.0", "2.0.0", false),
        ] {
            let status = mocked_check(ok_response(&manifest(remote, asset)), current)
                .await
                .unwrap();
            assert_eq!(status.available, available);
            assert_eq!(status.current_version, current);
            assert_eq!(status.version.as_deref(), available.then_some(remote));
            assert_eq!(status.notes.as_deref(), available.then_some("Update notes"));
            assert_eq!(
                status.pub_date.as_deref(),
                available.then_some("2026-11-27T00:00:00Z")
            );
        }
    }

    #[tokio::test]
    async fn reports_missing_and_invalid_releases() {
        let error = mocked_check(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            "1.3.0",
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            friendly_updater_error(CHECK_ACTION, "release not found")
        );

        let invalid = friendly_updater_error(CHECK_ACTION, "invalid release JSON");
        for body in [
            "{".to_owned(),
            manifest("2", "https://example.com/installer.exe"),
            manifest("2.0.0", "http://localhost/installer.exe"),
            manifest("2.0.0", "https://user@example.com/installer.exe"),
        ] {
            assert_eq!(
                mocked_check(ok_response(&body), "1.3.0").await.unwrap_err(),
                invalid
            );
        }

        let endpoint = "http://example.com/latest.json";
        assert!(
            check_for_updates_from_url(endpoint, DEFAULT_UPDATER_PUBKEY_B64, "1.3.0")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_insecure_redirects() {
        let response = concat!(
            "HTTP/1.1 302 Found\r\n",
            "Location: http://example.com/latest.json\r\n",
            "Content-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let error = mocked_check(response.into(), "1.3.0").await.unwrap_err();
        assert_eq!(error, friendly_updater_error(CHECK_ACTION, "network error"));
    }

    #[tokio::test]
    async fn enforces_manifest_limit_with_or_without_content_length() {
        let too_large = MANIFEST_LIMIT + 1;
        let oversized_header =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {too_large}\r\nConnection: close\r\n\r\n");
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{too_large:x}\r\n{}\r\n0\r\n\r\n",
            " ".repeat(too_large)
        );

        let expected = friendly_updater_error(CHECK_ACTION, "invalid release JSON");
        assert_eq!(
            mocked_check(oversized_header, "1.3.0").await.unwrap_err(),
            expected
        );
        assert_eq!(mocked_check(chunked, "1.3.0").await.unwrap_err(), expected);
    }
}
