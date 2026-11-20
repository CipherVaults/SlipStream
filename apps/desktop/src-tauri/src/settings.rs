use crate::paths::{canonical_directory, invalid, local_drive_path};
use serde::{de::DeserializeOwned, Serialize};
use slipstream_protocol::AppSettings;
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) const SETTINGS_FILE: &str = "settings.json";

pub(crate) fn read_json<T: DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error),
    }
}

pub(crate) fn write_json_atomically(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = path.with_file_name(format!(".slipstream-{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn normalize_tool_path(value: &mut Option<String>) -> io::Result<()> {
    if let Some(raw) = value.as_deref() {
        if raw.contains('\0') {
            return Err(invalid("tool path contains NUL byte"));
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            *value = None;
        } else {
            *value = Some(trimmed.to_owned());
        }
    }
    Ok(())
}

pub(crate) fn normalize(settings: &mut AppSettings, data_dir: &Path) -> io::Result<()> {
    settings.default_connections = settings.default_connections.clamp(1, 32);
    settings.max_concurrent_downloads = settings.max_concurrent_downloads.clamp(1, 32);
    settings.global_speed_limit_bps = settings.global_speed_limit_bps.filter(|&rate| rate != 0);
    normalize_tool_path(&mut settings.ytdlp_path)?;
    normalize_tool_path(&mut settings.ffmpeg_path)?;

    let path = if settings.default_download_dir.trim().is_empty() {
        env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("Downloads"))
            .filter(|dir| {
                local_drive_path(dir).is_ok()
                    && fs::create_dir_all(dir).is_ok()
                    && canonical_directory(dir).is_ok()
            })
            .unwrap_or_else(|| data_dir.join("Downloads"))
    } else {
        let raw = PathBuf::from(&settings.default_download_dir);
        let absolute = std::path::absolute(&raw)?;
        local_drive_path(&absolute)?
    };
    let clean = local_drive_path(&std::path::absolute(&path)?)?;
    fs::create_dir_all(&clean)?;
    let canonical = canonical_directory(&clean)?;
    let final_path = local_drive_path(&canonical)?;
    settings.default_download_dir = final_path
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "non-Unicode download folder"))?;
    Ok(())
}

pub(crate) fn torrent_rate(limit: Option<u64>) -> Option<u64> {
    limit
        .filter(|&rate| rate > 0)
        .map(|rate| rate.clamp(16_384, u64::from(u32::MAX)))
}
