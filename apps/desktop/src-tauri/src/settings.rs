use crate::paths::{ensure_safe_directory, invalid, local_drive_path, validate_configured_exe};
use serde::{de::DeserializeOwned, Serialize};
use slipstream_engine::numbered_filename;
use slipstream_protocol::{
    default_download_categories, sanitize_filename, AppSettings, DownloadCategory,
};
use std::{
    collections::HashSet,
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
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
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            *value = None;
        } else {
            let clean = validate_configured_exe(trimmed)?;
            *value = Some(
                clean
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("non-Unicode tool path"))?,
            );
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

    let canonical = if settings.default_download_dir.trim().is_empty() {
        env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|dir| dir.join("Downloads"))
            .and_then(|dir| ensure_safe_directory(&dir).ok())
            .map_or_else(|| ensure_safe_directory(&data_dir.join("Downloads")), Ok)?
    } else {
        ensure_safe_directory(Path::new(&settings.default_download_dir))?
    };
    let final_path = local_drive_path(&canonical)?;
    settings.default_download_dir = final_path
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "non-Unicode download folder"))?;
    normalize_categories(&mut settings.categories)
}

pub(crate) fn category_id(raw: &str) -> String {
    raw.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase()
}

pub(crate) fn normalize_categories(categories: &mut Vec<DownloadCategory>) -> io::Result<()> {
    if categories.is_empty() {
        *categories = default_download_categories();
        return Ok(());
    }
    let mut ids = HashSet::new();
    for category in categories.iter_mut() {
        category.name =
            sanitize_filename(&category.name).map_err(|error| invalid(error.to_string()))?;
        category.id = category_id(&category.id);
        if category.id.is_empty() {
            category.id = category_id(&category.name);
        }
        if category.id.is_empty() || !ids.insert(category.id.clone()) {
            return Err(invalid("category IDs must be non-empty and unique"));
        }
        let mut seen = HashSet::new();
        category.extensions = category
            .extensions
            .iter()
            .map(|extension| {
                extension
                    .trim()
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|extension| {
                !extension.is_empty() && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .filter(|extension| seen.insert(extension.clone()))
            .collect();
        category.save_dir = category
            .save_dir
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| {
                local_drive_path(&ensure_safe_directory(Path::new(path))?)?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid("non-Unicode category folder"))
            })
            .transpose()?;
    }
    if !ids.contains("general") {
        categories.insert(0, GENERAL_CATEGORY.clone());
    }
    Ok(())
}

static GENERAL_CATEGORY: LazyLock<DownloadCategory> = LazyLock::new(|| {
    default_download_categories()
        .into_iter()
        .next()
        .expect("default general category")
});

pub fn resolve_category_dir(settings: &AppSettings, category_id: &str) -> PathBuf {
    let base = PathBuf::from(&settings.default_download_dir);
    let Some(category) = settings
        .categories
        .iter()
        .find(|category| category.id == category_id)
    else {
        return base;
    };
    if let Some(directory) = &category.save_dir {
        PathBuf::from(directory)
    } else if category.id == "general" {
        base
    } else {
        base.join(&category.name)
    }
}

pub fn match_category_for_filename<'a>(
    settings: &'a AppSettings,
    filename: &str,
) -> &'a DownloadCategory {
    let extension = Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str());
    extension
        .and_then(|extension| {
            settings.categories.iter().find(|category| {
                category
                    .extensions
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(extension))
            })
        })
        .or_else(|| {
            settings
                .categories
                .iter()
                .find(|category| category.id == "general")
        })
        .unwrap_or(&GENERAL_CATEGORY)
}

pub fn next_numbered_filename(dir: &Path, filename: &str) -> String {
    let filename = sanitize_filename(filename).unwrap_or_else(|_| "download".into());
    for number in 0.. {
        let candidate = numbered_filename(&filename, number);
        if fs::symlink_metadata(dir.join(&candidate)).is_err() {
            return candidate;
        }
    }
    unreachable!("filename suffixes are unbounded")
}

pub(crate) fn torrent_rate(limit: Option<u64>) -> Option<u64> {
    limit
        .filter(|&rate| rate > 0)
        .map(|rate| rate.clamp(16_384, u64::from(u32::MAX)))
}
