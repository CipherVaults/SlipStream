use crate::paths::{
    canonical_directory, canonical_exe, ensure_safe_directory, invalid, local_drive_path,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slipstream_media::{args::validate_local_path, MediaToolchain, ToolchainConfig};
use slipstream_native_host::{
    manifest::{
        chromium_origin, valid_firefox_id, Browser, DEFAULT_CHROMIUM_EXTENSION_ID,
        DEFAULT_CHROMIUM_ORIGIN, DEFAULT_FIREFOX_ID,
    },
    registry::{host_key, install_hosts},
};
use slipstream_protocol::{
    AppSettings, BrowserKind, BrowserSetupResult, BrowserStatus, ToolSource, NATIVE_HOST_NAME,
};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    ptr::{null, null_mut},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
    time::timeout,
};
use url::Url;
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, ERROR_SUCCESS},
    Storage::FileSystem::{
        MoveFileExW, FILE_ATTRIBUTE_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH,
    },
    System::{
        Registry::{
            RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
            RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY,
        },
        Threading::CREATE_NO_WINDOW,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolchainStatus {
    pub ytdlp_path: Option<String>,
    pub ytdlp_version: Option<String>,
    #[serde(default)]
    pub ytdlp_source: ToolSource,
    pub ffmpeg_path: Option<String>,
    pub ffmpeg_version: Option<String>,
    #[serde(default)]
    pub ffmpeg_source: ToolSource,
    #[serde(default)]
    pub ffprobe_path: Option<String>,
    #[serde(default)]
    pub ffprobe_source: ToolSource,
    pub native_hosts_installed: bool,
}

type ToolResolution = io::Result<Option<(PathBuf, ToolSource)>>;

pub(crate) struct ToolchainResolution {
    pub ytdlp: ToolResolution,
    pub ffmpeg: ToolResolution,
    pub ffprobe: ToolResolution,
}

fn absolute_path_entries(path_env: Option<&OsStr>) -> impl Iterator<Item = PathBuf> + '_ {
    path_env
        .into_iter()
        .flat_map(env::split_paths)
        .filter(|dir| dir.is_absolute())
}

fn resolve_tool(
    name: &str,
    custom: Option<&Path>,
    managed_bin_dir: Option<&Path>,
    bundled_dirs: &[PathBuf],
    path_env: Option<&OsStr>,
) -> ToolResolution {
    if let Some(path) = custom {
        return canonical_exe(path).map(|path| Some((path, ToolSource::Custom)));
    }
    Ok(managed_bin_dir
        .into_iter()
        .map(|dir| (dir, ToolSource::Managed))
        .chain(
            bundled_dirs
                .iter()
                .map(|dir| (dir.as_path(), ToolSource::Bundled)),
        )
        .find_map(|(dir, source)| {
            canonical_exe(&dir.join(name))
                .ok()
                .map(|path| (path, source))
        })
        .or_else(|| {
            absolute_path_entries(path_env).find_map(|dir| {
                canonical_exe(&dir.join(name))
                    .ok()
                    .map(|path| (path, ToolSource::SystemPath))
            })
        }))
}

pub(crate) fn resolve_toolchain_with_dirs(
    settings: &AppSettings,
    managed_bin_dir: Option<&Path>,
    bundled_dirs: &[PathBuf],
    path_env: Option<&OsStr>,
) -> ToolchainResolution {
    fn configured(raw: Option<&str>) -> Option<&Path> {
        raw.map(str::trim).filter(|s| !s.is_empty()).map(Path::new)
    }
    let find = |name: &str, custom: Option<&Path>| {
        resolve_tool(name, custom, managed_bin_dir, bundled_dirs, path_env)
    };
    let custom_ffmpeg = configured(settings.ffmpeg_path.as_deref());
    let ytdlp = find("yt-dlp.exe", configured(settings.ytdlp_path.as_deref()));
    let ffmpeg = find("ffmpeg.exe", custom_ffmpeg);
    let ffprobe = (|| {
        if custom_ffmpeg.is_some() {
            let sibling = ffmpeg
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .and_then(|(path, _)| path.parent())
                .ok_or_else(|| invalid("configured FFmpeg path is invalid"))?
                .join("ffprobe.exe");
            match fs::symlink_metadata(&sibling) {
                Ok(_) => return find("ffprobe.exe", Some(&sibling)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        find("ffprobe.exe", None)
    })();
    ToolchainResolution {
        ytdlp,
        ffmpeg,
        ffprobe,
    }
}

#[cfg(test)]
pub(crate) fn hardened_toolchain_config_with_path(
    settings: &AppSettings,
    path_env: Option<&OsStr>,
) -> io::Result<ToolchainConfig> {
    hardened_toolchain_config_with_dirs(settings, None, &[], path_env)
}

pub(crate) fn hardened_toolchain_config_with_dirs(
    settings: &AppSettings,
    managed_bin_dir: Option<&Path>,
    bundled_dirs: &[PathBuf],
    path_env: Option<&OsStr>,
) -> io::Result<ToolchainConfig> {
    resolve_toolchain_with_dirs(settings, managed_bin_dir, bundled_dirs, path_env).config(path_env)
}

impl ToolchainResolution {
    fn config(&self, path_env: Option<&OsStr>) -> io::Result<ToolchainConfig> {
        let path = |tool: &ToolResolution| {
            tool.as_ref()
                .map(|resolved| resolved.as_ref().map(|(path, _)| path.clone()))
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        };
        let missing =
            |name| io::Error::new(io::ErrorKind::NotFound, format!("{name} was not found"));
        let ytdlp = path(&self.ytdlp)?.ok_or_else(|| missing("yt-dlp.exe"))?;
        let ffmpeg = path(&self.ffmpeg)?.ok_or_else(|| missing("ffmpeg.exe"))?;
        let ffprobe = path(&self.ffprobe)?;
        // When ffprobe is absent, the shared media crate searches the FFmpeg
        // sibling and PATH itself. Reject any file that lookup could pick up
        // after the desktop resolver has excluded it.
        if ffprobe.is_none()
            && ffmpeg
                .parent()
                .into_iter()
                .map(Path::to_owned)
                .chain(absolute_path_entries(path_env))
                .any(|dir| dir.join("ffprobe.exe").is_file())
        {
            return Err(invalid(
                "ffprobe fallback path is not a safe local executable",
            ));
        }
        Ok(ToolchainConfig {
            ytdlp_path: Some(ytdlp),
            ffmpeg_path: Some(ffmpeg),
            ffprobe_path: ffprobe,
            ..ToolchainConfig::default()
        })
    }
}

pub(crate) fn default_managed_bin_dir() -> Option<PathBuf> {
    slipstream_native_host::token::local_app_directory()
        .ok()
        .map(|dir| dir.join("bin"))
}

fn bundled_tool_dirs(resource_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe_dir) = env::current_exe().ok().as_deref().and_then(Path::parent) {
        dirs.extend([exe_dir.join("bin"), exe_dir.to_owned()]);
    }
    if let Some(dir) = resource_dir {
        dirs.extend([
            dir.join("bin"),
            dir.join("resources").join("bin"),
            dir.to_owned(),
        ]);
    }
    dirs
}

fn hardened_toolchain_config(
    settings: &AppSettings,
    resource_dir: Option<&Path>,
) -> io::Result<ToolchainConfig> {
    let path_env = env::var_os("PATH");
    hardened_toolchain_config_with_dirs(
        settings,
        default_managed_bin_dir().as_deref(),
        &bundled_tool_dirs(resource_dir),
        path_env.as_deref(),
    )
}

pub(crate) async fn discover_media_toolchain(
    settings: AppSettings,
    resource_dir: Option<PathBuf>,
) -> std::result::Result<MediaToolchain, String> {
    let config = tokio::task::spawn_blocking(move || {
        hardened_toolchain_config(&settings, resource_dir.as_deref())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    MediaToolchain::discover(config)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn toolchain_status(
    settings: &AppSettings,
    base_key: &str,
    resource_dir: Option<&Path>,
) -> ToolchainStatus {
    let settings = settings.clone();
    let resource_dir = resource_dir.map(Path::to_owned);
    let base_key = base_key.to_owned();
    let (resolution, config, native_hosts_installed) = tokio::task::spawn_blocking(move || {
        let path_env = env::var_os("PATH");
        let resolution = resolve_toolchain_with_dirs(
            &settings,
            default_managed_bin_dir().as_deref(),
            &bundled_tool_dirs(resource_dir.as_deref()),
            path_env.as_deref(),
        );
        let config = resolution.config(path_env.as_deref()).ok();
        (resolution, config, native_hosts_installed(&base_key))
    })
    .await
    .unwrap_or((
        ToolchainResolution {
            ytdlp: Ok(None),
            ffmpeg: Ok(None),
            ffprobe: Ok(None),
        },
        None,
        false,
    ));

    let discovered = match config {
        Some(config) => MediaToolchain::discover(config).await.ok(),
        None => None,
    };
    let ytdlp = resolution.ytdlp.ok().flatten();
    let ffmpeg = resolution.ffmpeg.ok().flatten();
    let ffprobe = resolution.ffprobe.ok().flatten();
    let (ytdlp_version, ffmpeg_version) = if let Some(tools) = discovered {
        (
            tools
                .ytdlp()
                .and_then(|binary| parse_ytdlp_version(&binary.version)),
            parse_ffmpeg_version(&tools.ffmpeg().version),
        )
    } else {
        // The shared toolchain requires FFmpeg even for yt-dlp. Probe each
        // binary alone here when the full toolchain cannot be constructed.
        tokio::join!(
            async {
                match &ytdlp {
                    Some((path, _)) => probe_version(path, "--version", parse_ytdlp_version).await,
                    None => None,
                }
            },
            async {
                match &ffmpeg {
                    Some((path, _)) => probe_version(path, "-version", parse_ffmpeg_version).await,
                    None => None,
                }
            },
        )
    };
    let fields = |binary: Option<(PathBuf, ToolSource)>| {
        let Some((path, source)) = binary else {
            return (None, ToolSource::Missing);
        };
        let path = local_drive_path(&path).unwrap_or(path);
        (Some(path.to_string_lossy().into_owned()), source)
    };
    let (ytdlp_path, ytdlp_source) = fields(ytdlp);
    let (ffmpeg_path, ffmpeg_source) = fields(ffmpeg);
    let (ffprobe_path, ffprobe_source) = fields(ffprobe);
    ToolchainStatus {
        ytdlp_path,
        ytdlp_version,
        ytdlp_source,
        ffmpeg_path,
        ffmpeg_version,
        ffmpeg_source,
        ffprobe_path,
        ffprobe_source,
        native_hosts_installed,
    }
}

fn first_nonempty_line(raw: &str) -> Option<&str> {
    raw.lines().map(str::trim).find(|line| !line.is_empty())
}

pub(crate) fn parse_ytdlp_version(raw: &str) -> Option<String> {
    let line = first_nonempty_line(raw)?;
    let token = line.split_whitespace().next()?;
    let date = matches!(token.as_bytes(), [a, b, c, d, b'.', ..]
        if [a, b, c, d].into_iter().all(u8::is_ascii_digit));
    Some(if date { token } else { line }.to_owned())
}

pub(crate) fn parse_ffmpeg_version(raw: &str) -> Option<String> {
    let line = first_nonempty_line(raw)?;
    let Some(build) = line
        .strip_prefix("ffmpeg version ")
        .or_else(|| line.strip_prefix("ffprobe version "))
    else {
        return Some(line.to_owned());
    };
    let build = build
        .split_once(" Copyright (c)")
        .map_or(build, |(build, _)| build)
        .trim();
    (!build.is_empty()).then(|| build.to_owned())
}

async fn probe_version(
    path: &Path,
    flag: &str,
    parse: fn(&str) -> Option<String>,
) -> Option<String> {
    let path = canonical_exe(path).ok()?;
    validate_local_path(&path).ok()?;
    let mut child = Command::new(&path)
        .arg(flag)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("YTDLP_NO_PLUGINS", "1")
        .env_remove("FFREPORT")
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(65_537);
    let mut output = Vec::new();
    let status = timeout(Duration::from_secs(8), async {
        stdout.read_to_end(&mut output).await?;
        drop(stdout);
        child.wait().await
    })
    .await
    .ok()?
    .ok()?;
    if !status.success() || output.len() > 65_536 {
        return None;
    }
    parse(&String::from_utf8_lossy(&output))
}

const YTDLP_RELEASE_BASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
const YTDLP_EXE: &str = "yt-dlp.exe";
const YTDLP_CHECKSUMS: &str = "SHA2-256SUMS";
const MAX_CHECKSUMS_BYTES: usize = 1024 * 1024;
const MAX_YTDLP_BYTES: usize = 128 * 1024 * 1024;
static YTDLP_UPDATE: Mutex<()> = Mutex::const_new(());

pub(crate) fn allowed_ytdlp_release_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        )
}

pub(crate) fn parse_sha256sums_for_file(contents: &str, filename: &str) -> io::Result<[u8; 32]> {
    let mut found = None;
    for line in contents.lines() {
        let Some((hash, listed_name)) = line.trim().split_once(char::is_whitespace) else {
            if line.trim() == filename {
                return Err(invalid(format!(
                    "Malformed SHA-256 checksum for {filename}"
                )));
            }
            continue;
        };
        let listed_name = listed_name.trim_start();
        let listed_name = listed_name.strip_prefix('*').unwrap_or(listed_name);
        if listed_name.strip_prefix("./").unwrap_or(listed_name) != filename {
            continue;
        }
        let mut digest = [0u8; 32];
        hex::decode_to_slice(hash, &mut digest)
            .map_err(|_| invalid(format!("Malformed SHA-256 checksum for {filename}")))?;
        if found.is_some_and(|previous| previous != digest) {
            return Err(invalid(format!(
                "Conflicting SHA-256 checksums for {filename}"
            )));
        }
        found = Some(digest);
    }
    found.ok_or_else(|| invalid(format!("SHA-256 checksum for {filename} is missing")))
}

pub(crate) async fn update_ytdlp_managed() -> Result<(PathBuf, String), String> {
    let directory = default_managed_bin_dir()
        .ok_or("SlipStream could not find its local app data directory.")?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 10 {
                attempt.error("Too many redirects for the yt-dlp release")
            } else if !allowed_ytdlp_release_url(attempt.url()) {
                attempt.error("Untrusted redirect for the yt-dlp release")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| format!("Could not initialize the yt-dlp updater: {error}"))?;
    update_ytdlp_from_base_url(YTDLP_RELEASE_BASE, &client, &directory).await
}

pub(crate) async fn update_ytdlp_from_base_url(
    base_url: &str,
    client: &reqwest::Client,
    managed_bin_dir: &Path,
) -> Result<(PathBuf, String), String> {
    let _update = YTDLP_UPDATE.lock().await;
    let path = install_ytdlp_from_base_url(base_url, client, managed_bin_dir).await?;
    let version = probe_version(&path, "--version", parse_ytdlp_version)
        .await
        .unwrap_or_default();
    Ok((path, version))
}

async fn release_asset(
    base_url: &str,
    client: &reqwest::Client,
    name: &str,
    size_limit: usize,
) -> Result<reqwest::Response, String> {
    let response = client
        .get(format!("{}/{name}", base_url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|error| format!("Could not download {name}: {}", error.without_url()))?;
    if !response.status().is_success() {
        return Err(format!(
            "Could not download {name}: HTTP {}",
            response.status()
        ));
    }
    check_download_size(name, response.content_length().unwrap_or(0), size_limit)?;
    Ok(response)
}

fn check_download_size(name: &str, bytes: u64, limit: usize) -> Result<(), String> {
    if bytes > limit as u64 {
        return Err(format!("{name} exceeds the download size limit."));
    }
    Ok(())
}

fn regular_non_reparse_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

fn clean_stale_ytdlp_downloads(directory: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with(".yt-dlp-") && name.ends_with(".tmp"))
            && regular_non_reparse_file(&fs::symlink_metadata(entry.path())?)
        {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

struct YtdlpStagingFile(PathBuf);

impl Drop for YtdlpStagingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

async fn install_ytdlp_from_base_url(
    base_url: &str,
    client: &reqwest::Client,
    managed_bin_dir: &Path,
) -> Result<PathBuf, String> {
    let directory = ensure_safe_directory(managed_bin_dir)
        .map_err(|error| format!("Could not prepare the managed tool directory: {error}"))?;
    clean_stale_ytdlp_downloads(&directory)
        .map_err(|error| format!("Could not clean up a previous yt-dlp download: {error}"))?;

    let mut sums = release_asset(base_url, client, YTDLP_CHECKSUMS, MAX_CHECKSUMS_BYTES)
        .await?
        .bytes_stream();
    let mut contents = Vec::new();
    while let Some(chunk) = sums.next().await {
        let chunk = chunk.map_err(|_| "The yt-dlp checksum download was interrupted.")?;
        check_download_size(
            YTDLP_CHECKSUMS,
            (contents.len() + chunk.len()) as u64,
            MAX_CHECKSUMS_BYTES,
        )?;
        contents.extend_from_slice(&chunk);
    }
    let contents = std::str::from_utf8(&contents)
        .map_err(|_| "The yt-dlp checksum file is not valid UTF-8.")?;
    let expected =
        parse_sha256sums_for_file(contents, YTDLP_EXE).map_err(|error| error.to_string())?;

    let mut download = release_asset(base_url, client, YTDLP_EXE, MAX_YTDLP_BYTES)
        .await?
        .bytes_stream();
    let staging_path = directory.join(format!(".yt-dlp-{}.tmp", Uuid::new_v4()));
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(&staging_path)
        .await
        .map_err(|error| format!("Could not create the yt-dlp download file: {error}"))?;
    let staging = YtdlpStagingFile(staging_path);
    let mut file = file;
    let written = async {
        let mut bytes = 0;
        let mut hash = Sha256::new();
        while let Some(chunk) = download.next().await {
            let chunk = chunk.map_err(|_| "The yt-dlp download was interrupted.")?;
            bytes += chunk.len() as u64;
            check_download_size(YTDLP_EXE, bytes, MAX_YTDLP_BYTES)?;
            file.write_all(&chunk)
                .await
                .map_err(|error| format!("Could not write the yt-dlp download: {error}"))?;
            hash.update(&chunk);
        }
        if <[u8; 32]>::from(hash.finalize()) != expected {
            return Err("The yt-dlp download failed SHA-256 verification.".into());
        }
        file.flush()
            .await
            .map_err(|error| format!("Could not flush the yt-dlp download: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("Could not save the yt-dlp download: {error}"))
    }
    .await;
    // A pending Tokio write can keep the Windows handle open after a failed download.
    drop(file.into_std().await);
    written?;

    let installed = directory.join(YTDLP_EXE);
    replace_ytdlp(&directory, &staging.0, &installed)?;
    canonical_exe(&installed)
        .map_err(|error| format!("Could not validate the installed yt-dlp: {error}"))
}

fn replace_ytdlp(directory: &Path, staging_path: &Path, installed: &Path) -> Result<(), String> {
    let validation_error =
        |error| format!("Could not validate the yt-dlp installation paths: {error}");
    canonical_directory(directory).map_err(validation_error)?;
    if !regular_non_reparse_file(&fs::symlink_metadata(staging_path).map_err(validation_error)?) {
        return Err("The yt-dlp download file is not a regular local file.".into());
    }
    match fs::symlink_metadata(installed) {
        Ok(_) => {
            canonical_exe(installed).map_err(|error| {
                if locked_executable(&error) {
                    ytdlp_in_use_error()
                } else {
                    validation_error(error)
                }
            })?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) if !locked_executable(&error) => return Err(validation_error(error)),
        Err(_) => return Err(ytdlp_in_use_error()),
    }
    let from = wide(staging_path).map_err(validation_error)?;
    let to = wide(installed).map_err(validation_error)?;
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return Err(if locked_executable(&error) {
            ytdlp_in_use_error()
        } else {
            format!("Could not replace yt-dlp: {error}")
        });
    }
    Ok(())
}

fn locked_executable(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || error.raw_os_error().is_some_and(|code| {
            code == ERROR_SHARING_VIOLATION as i32 || code == ERROR_LOCK_VIOLATION as i32
        })
}

fn ytdlp_in_use_error() -> String {
    "Cannot update yt-dlp while a media download is using it. Pause or wait for active media downloads to finish and try again.".into()
}

pub(crate) fn native_host_executable(resource_dir: Option<&Path>) -> io::Result<PathBuf> {
    let current = env::current_exe()?;
    let executable_dir = current
        .parent()
        .ok_or_else(|| invalid("desktop executable has no parent"))?;
    let mut dirs = vec![executable_dir.to_owned()];
    if let Some(dir) = resource_dir {
        dirs.extend([dir.to_owned(), dir.join("bin")]);
    }
    if executable_dir.file_name() == Some(OsStr::new("deps")) {
        if let Some(parent) = executable_dir.parent() {
            dirs.push(parent.to_owned());
        }
    }
    dirs.into_iter()
        .find_map(|dir| canonical_exe(&dir.join("slipstream-native-host.exe")).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "slipstream-native-host.exe is missing",
            )
        })
}

pub(crate) fn explorer_executable() -> io::Result<PathBuf> {
    let system_root = env::var_os("SystemRoot")
        .or_else(|| env::var_os("WINDIR"))
        .map(PathBuf::from)
        .ok_or_else(|| invalid("SystemRoot environment variable is unavailable"))?;
    let system32 = system_root.join("explorer.exe");
    canonical_exe(&system32)
}

pub fn install_browser_hosts_into(
    base_key: &str,
    directory: &Path,
    executable: &Path,
    chrome_id: Option<&str>,
    edge_id: Option<&str>,
    firefox_id: Option<&str>,
) -> io::Result<Vec<PathBuf>> {
    let executable = local_drive_path(&canonical_exe(executable)?)?;
    validate_local_path(&executable).map_err(|error| invalid(error.to_string()))?;
    let mut origins = Vec::new();
    for id in [chrome_id, edge_id]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let origin = extension_origin(id)?;
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    if origins.is_empty() {
        origins.push(DEFAULT_CHROMIUM_ORIGIN.to_owned());
    }
    let firefox = firefox_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(DEFAULT_FIREFOX_ID);
    if !valid_firefox_id(firefox) {
        return Err(invalid("expected a valid Firefox extension ID"));
    }
    let directory = std::path::absolute(directory)?;
    for browser in Browser::ALL {
        ensure_safe_directory(&directory.join(browser.directory()))?;
    }
    install_hosts(
        base_key,
        &directory,
        &executable,
        &origins,
        &[firefox.to_owned()],
    )
}

fn extension_origin(id: &str) -> io::Result<String> {
    if id.starts_with("chrome-extension://") {
        chromium_origin(id)
    } else {
        if id.len() != 32 {
            return Err(invalid("expected a 32-character Chromium extension ID"));
        }
        chromium_origin(&format!("chrome-extension://{id}"))
    }
}

fn wide(value: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut units: Vec<u16> = value.as_ref().encode_wide().collect();
    if units.contains(&0) {
        return Err(invalid("embedded NUL in registry string"));
    }
    units.push(0);
    Ok(units)
}

const BROWSERS: [BrowserKind; 4] = [
    BrowserKind::Chrome,
    BrowserKind::Edge,
    BrowserKind::Brave,
    BrowserKind::Firefox,
];

fn host_browser(kind: BrowserKind) -> Browser {
    match kind {
        BrowserKind::Chrome => Browser::Chrome,
        BrowserKind::Edge => Browser::Edge,
        BrowserKind::Brave => Browser::Brave,
        BrowserKind::Firefox => Browser::Firefox,
    }
}

pub(crate) fn browser_metadata(
    kind: BrowserKind,
) -> (&'static str, &'static str, &'static str, &'static str) {
    match kind {
        BrowserKind::Chrome => (
            "Google Chrome",
            "chrome://extensions",
            DEFAULT_CHROMIUM_EXTENSION_ID,
            "chrome",
        ),
        BrowserKind::Edge => (
            "Microsoft Edge",
            "edge://extensions",
            DEFAULT_CHROMIUM_EXTENSION_ID,
            "chrome",
        ),
        BrowserKind::Brave => (
            "Brave",
            "brave://extensions",
            DEFAULT_CHROMIUM_EXTENSION_ID,
            "chrome",
        ),
        BrowserKind::Firefox => (
            "Mozilla Firefox",
            "about:debugging#/runtime/this-firefox",
            DEFAULT_FIREFOX_ID,
            "firefox",
        ),
    }
}

pub(crate) fn detect_browser_executable(kind: BrowserKind) -> Option<PathBuf> {
    let (exe, location) = match kind {
        BrowserKind::Chrome => ("chrome.exe", "Google/Chrome/Application"),
        BrowserKind::Edge => ("msedge.exe", "Microsoft/Edge/Application"),
        BrowserKind::Brave => ("brave.exe", "BraveSoftware/Brave-Browser/Application"),
        BrowserKind::Firefox => ("firefox.exe", "Mozilla Firefox"),
    };
    let key = format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{exe}");
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .flat_map(|hive| [0, RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY].map(|view| (hive, view)))
        .filter_map(|(hive, view)| registry_default_path(hive, &key, view))
        .find_map(|path| canonical_exe(&path).ok())
        .or_else(|| {
            ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
                .into_iter()
                .filter_map(env::var_os)
                .find_map(|dir| canonical_exe(&PathBuf::from(dir).join(location).join(exe)).ok())
        })
}

pub(crate) fn browser_native_host_registered(base_key: &str, kind: BrowserKind) -> bool {
    let key = host_key(base_key, host_browser(kind));
    let manifest = (|| {
        let path = registry_default_path(HKEY_CURRENT_USER, &key, 0)?;
        let parent = canonical_directory(path.parent()?).ok()?;
        let path = parent.join(path.file_name()?);
        if !regular_non_reparse_file(&fs::symlink_metadata(&path).ok()?) {
            return None;
        }
        const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
        let mut data = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut data)
            .ok()?;
        if data.len() as u64 > MAX_MANIFEST_BYTES {
            return None;
        }
        serde_json::from_slice::<serde_json::Value>(&data).ok()
    })();
    manifest.is_some_and(|json| {
        if json["name"] != NATIVE_HOST_NAME
            || json["type"] != "stdio"
            || !json["path"]
                .as_str()
                .is_some_and(|path| canonical_exe(Path::new(path)).is_ok())
        {
            return false;
        }
        let allowed = if kind == BrowserKind::Firefox {
            "allowed_extensions"
        } else {
            "allowed_origins"
        };
        json[allowed]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .any(|id| {
                if kind == BrowserKind::Firefox {
                    valid_firefox_id(id)
                } else {
                    chromium_origin(id).is_ok()
                }
            })
    })
}

pub(crate) fn native_hosts_installed(base_key: &str) -> bool {
    BROWSERS
        .into_iter()
        .any(|kind| browser_native_host_registered(base_key, kind))
}

fn registry_default_path(hive: HKEY, key: &str, flags: u32) -> Option<PathBuf> {
    let key = wide(key).ok()?;
    let mut buffer = vec![0u16; 32_768];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            hive,
            key.as_ptr(),
            null(),
            RRF_RT_REG_SZ | flags,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS
        || bytes < 2
        || !bytes.is_multiple_of(2)
        || bytes as usize > buffer.len() * 2
    {
        return None;
    }
    let (0, path) = buffer[..bytes as usize / 2].split_last()? else {
        return None;
    };
    if path.contains(&0) {
        return None;
    }
    let path = PathBuf::from(OsString::from_wide(path));
    local_drive_path(&path).ok()?;
    Some(path)
}

pub(crate) fn resolve_bundled_extension_dir(
    kind: BrowserKind,
    resource_dir: Option<&Path>,
) -> Option<PathBuf> {
    let current_exe = env::current_exe().ok();
    resolve_bundled_extension_dir_with_roots(
        kind,
        current_exe.as_deref().and_then(Path::parent),
        resource_dir,
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

pub(crate) fn resolve_bundled_extension_dir_with_roots(
    kind: BrowserKind,
    exe_dir: Option<&Path>,
    resource_dir: Option<&Path>,
    manifest_dir: &Path,
) -> Option<PathBuf> {
    let (_, _, _, subdir) = browser_metadata(kind);
    exe_dir
        .into_iter()
        .map(|dir| dir.join("extensions").join(subdir))
        .chain(resource_dir.into_iter().flat_map(|dir| {
            [
                dir.join("extensions").join(subdir),
                dir.join("resources/extensions").join(subdir),
            ]
        }))
        .chain(
            exe_dir
                .into_iter()
                .chain([manifest_dir])
                .flat_map(Path::ancestors)
                .map(|dir| dir.join("extensions/browser/dist").join(subdir)),
        )
        .find_map(|candidate| {
            let canonical = canonical_directory(&candidate).ok()?;
            let metadata = fs::symlink_metadata(canonical.join("manifest.json")).ok()?;
            if !regular_non_reparse_file(&metadata) {
                return None;
            }
            local_drive_path(&canonical).ok()
        })
}

pub(crate) fn list_browsers(base_key: &str, resource_dir: Option<&Path>) -> Vec<BrowserStatus> {
    BROWSERS
        .into_iter()
        .map(|id| {
            let (name, extension_url, extension_id, _) = browser_metadata(id);
            BrowserStatus {
                id,
                name: name.to_owned(),
                installed: detect_browser_executable(id).is_some(),
                native_host_registered: browser_native_host_registered(base_key, id),
                extension_url: extension_url.to_owned(),
                bundled_extension_dir: resolve_bundled_extension_dir(id, resource_dir)
                    .map(|dir| dir.to_string_lossy().into_owned()),
                extension_id: extension_id.to_owned(),
            }
        })
        .collect()
}

pub(crate) fn open_browser_extension_setup(
    base_key: &str,
    manifests: &Path,
    resource_dir: Option<&Path>,
    browser_id: BrowserKind,
    custom_extension_id: Option<&str>,
) -> io::Result<BrowserSetupResult> {
    let (name, page, default_id, _) = browser_metadata(browser_id);
    let firefox = browser_id == BrowserKind::Firefox;
    let extension_id = match custom_extension_id.map(str::trim) {
        None => default_id.to_owned(),
        Some(id) if !firefox => extension_origin(id)?
            .trim_start_matches("chrome-extension://")
            .trim_end_matches('/')
            .to_owned(),
        Some(id) if valid_firefox_id(id) => id.to_owned(),
        Some(_) => return Err(invalid("expected a valid Firefox extension ID")),
    };
    if let Ok(executable) = native_host_executable(resource_dir) {
        install_browser_hosts_into(
            base_key,
            manifests,
            &executable,
            (!firefox && custom_extension_id.is_some()).then_some(extension_id.as_str()),
            None,
            (firefox && custom_extension_id.is_some()).then_some(extension_id.as_str()),
        )?;
    }
    let extension_dir =
        resolve_bundled_extension_dir(browser_id, resource_dir).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("The SlipStream extension folder for {name} is missing."),
            )
        })?;
    StdCommand::new(explorer_executable()?)
        .arg(&extension_dir)
        .spawn()
        .map_err(|error| {
            io::Error::other(format!(
                "Could not open the extension folder in File Explorer: {error}"
            ))
        })?;
    let launched_browser = detect_browser_executable(browser_id)
        .is_some_and(|path| StdCommand::new(path).arg(page).spawn().is_ok());
    let instructions = if firefox {
        format!("1. Open {page} in {name}.\n2. Click Load Temporary Add-on.\n3. Select manifest.json in the folder opened in File Explorer.")
    } else {
        format!("1. Open {page} in {name}.\n2. Enable Developer mode.\n3. Click Load unpacked and select the folder opened in File Explorer.")
    };
    Ok(BrowserSetupResult {
        browser_id,
        launched_browser,
        revealed_extension_dir: true,
        native_host_registered: browser_native_host_registered(base_key, browser_id),
        extension_dir: extension_dir.to_string_lossy().into_owned(),
        extension_page_url: page.to_owned(),
        extension_id,
        instructions,
    })
}

pub(crate) fn defender_executable() -> Option<PathBuf> {
    let version = |name: &OsStr| -> Option<Vec<u32>> {
        name.to_str()?
            .split(['.', '-'])
            .map(|number| number.parse().ok())
            .collect()
    };
    env::var_os("ProgramData")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .and_then(|dir| fs::read_dir(dir.join("Microsoft/Windows Defender/Platform")).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let version = version(&entry.file_name())?;
            Some((
                version,
                canonical_exe(&entry.path().join("MpCmdRun.exe")).ok()?,
            ))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path)| path)
        .or_else(|| {
            let dir = PathBuf::from(env::var_os("ProgramFiles")?);
            if !dir.is_absolute() {
                return None;
            }
            canonical_exe(&dir.join("Windows Defender/MpCmdRun.exe")).ok()
        })
}
