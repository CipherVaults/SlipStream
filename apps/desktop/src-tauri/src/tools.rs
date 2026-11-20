use crate::{
    paths::{canonical_exe, invalid, local_drive_path},
    settings::write_json_atomically,
};
use serde::{Deserialize, Serialize};
use slipstream_media::{args::validate_local_path, MediaToolchain, ToolchainConfig};
use slipstream_native_host::{
    manifest::{chromium_origin, generate, Browser, DEFAULT_FIREFOX_ID},
    registry::{host_key, install_hosts, DEFAULT_BASE_KEY},
};
use slipstream_protocol::{AppSettings, NATIVE_HOST_NAME};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Stdio,
    ptr::{null, null_mut},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    System::{
        Registry::{
            RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW, HKEY_CURRENT_USER,
            KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
        },
        Threading::CREATE_NO_WINDOW,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolchainStatus {
    pub ytdlp_path: Option<String>,
    pub ytdlp_version: Option<String>,
    pub ffmpeg_path: Option<String>,
    pub ffmpeg_version: Option<String>,
    pub native_hosts_installed: bool,
}

pub(crate) async fn toolchain_status(settings: &AppSettings) -> ToolchainStatus {
    let config = ToolchainConfig::from(settings);
    let discovery = MediaToolchain::discover(config.clone()).await;
    let (ytdlp, ffmpeg) = if let Ok(tools) = discovery {
        (
            tools
                .ytdlp()
                .map(|binary| (binary.path.clone(), binary.version.clone())),
            Some((tools.ffmpeg().path.clone(), tools.ffmpeg().version.clone())),
        )
    } else {
        // The shared toolchain requires FFmpeg even for yt-dlp. Probe each
        // binary alone here when the full toolchain cannot be constructed.
        tokio::join!(
            probe_version(config.ytdlp_path.as_deref(), "yt-dlp.exe", "--version"),
            probe_version(config.ffmpeg_path.as_deref(), "ffmpeg.exe", "-version"),
        )
    };
    let fields = |binary: Option<(PathBuf, String)>| {
        let Some((path, version)) = binary else {
            return (None, None);
        };
        let path = local_drive_path(&path).unwrap_or(path);
        (Some(path.to_string_lossy().into_owned()), Some(version))
    };
    let (ytdlp_path, ytdlp_version) = fields(ytdlp);
    let (ffmpeg_path, ffmpeg_version) = fields(ffmpeg);
    ToolchainStatus {
        ytdlp_path,
        ytdlp_version,
        ffmpeg_path,
        ffmpeg_version,
        native_hosts_installed: native_hosts_installed(DEFAULT_BASE_KEY),
    }
}

async fn probe_version(path: Option<&Path>, name: &str, flag: &str) -> Option<(PathBuf, String)> {
    let path = match path {
        Some(path) => canonical_exe(path).ok()?,
        None => env::var_os("PATH")
            .iter()
            .flat_map(env::split_paths)
            .filter(|dir| dir.is_absolute())
            .find_map(|dir| canonical_exe(&dir.join(name)).ok())?,
    };
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
    let output = String::from_utf8_lossy(&output);
    let version = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some((path, version.into()))
}

pub(crate) fn native_host_executable(resource_dir: &Path) -> io::Result<PathBuf> {
    let current = env::current_exe()?;
    let executable_dir = current
        .parent()
        .ok_or_else(|| invalid("desktop executable has no parent"))?;
    let mut dirs = vec![
        executable_dir.to_owned(),
        resource_dir.to_owned(),
        resource_dir.join("bin"),
    ];
    if executable_dir.file_name() == Some(OsStr::new("deps")) {
        if let Some(parent) = executable_dir.parent() {
            dirs.push(parent.to_owned());
        }
    }
    if let Some(path_var) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path_var).filter(|dir| dir.is_absolute()));
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
        let origin = if id.starts_with("chrome-extension://") {
            chromium_origin(id)?
        } else {
            if id.len() != 32 {
                return Err(invalid("expected a 32-character Chromium extension ID"));
            }
            chromium_origin(&format!("chrome-extension://{id}/"))?
        };
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    let firefox = [firefox_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(DEFAULT_FIREFOX_ID)
        .to_owned()];
    if !origins.is_empty() {
        return install_hosts(base_key, directory, &executable, &origins, &firefox);
    }

    let manifest = generate(Browser::Firefox, &executable, &[], &firefox)?;
    let parent = std::path::absolute(directory)?.join(Browser::Firefox.directory());
    fs::create_dir_all(&parent)?;
    let path = parent.join(format!("{NATIVE_HOST_NAME}.json"));
    write_json_atomically(&path, &manifest)?;
    register_firefox(base_key, &path)?;
    Ok(vec![path])
}

fn wide(value: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut units: Vec<u16> = value.as_ref().encode_wide().collect();
    if units.contains(&0) {
        return Err(invalid("embedded NUL in registry string"));
    }
    units.push(0);
    Ok(units)
}

fn registry_result(status: u32) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn register_firefox(base_key: &str, manifest_path: &Path) -> io::Result<()> {
    let key_path = wide(host_key(base_key, Browser::Firefox))?;
    let value = wide(manifest_path)?;
    let mut key = null_mut();
    registry_result(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null(),
            &mut key,
            null_mut(),
        )
    })?;
    let status = unsafe {
        let status = RegSetValueExW(
            key,
            null(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        );
        RegCloseKey(key);
        status
    };
    registry_result(status)
}

pub(crate) fn native_hosts_installed(base_key: &str) -> bool {
    Browser::ALL.into_iter().any(|browser| {
        installed_manifest(base_key, browser)
            .and_then(|path| fs::File::open(path).ok())
            .and_then(|file| {
                serde_json::from_reader::<_, serde_json::Value>(std::io::Read::take(
                    file,
                    1024 * 1024,
                ))
                .ok()
            })
            .is_some_and(|json| {
                json["name"] == NATIVE_HOST_NAME
                    && json["type"] == "stdio"
                    && json["path"]
                        .as_str()
                        .is_some_and(|path| canonical_exe(Path::new(path)).is_ok())
            })
    })
}

fn installed_manifest(base_key: &str, browser: Browser) -> Option<PathBuf> {
    let key = wide(host_key(base_key, browser)).ok()?;
    let mut buffer = vec![0u16; 32_768];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            null(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS || bytes < 2 || bytes as usize > buffer.len() * 2 {
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
