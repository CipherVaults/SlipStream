use crate::storage::Record;
use crate::{EngineError, Result};
use md5::Md5;
use sha2::{Digest, Sha256};
use slipstream_protocol::security::MAX_FILENAME_BYTES;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
#[cfg(windows)]
use std::io::Write;
use std::path::Path;

pub(crate) fn regular_file_length(path: &Path) -> Result<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !is_reparse_point(&meta) => Ok(Some(meta.len())),
        Ok(_) => Err(EngineError::InvalidState(
            "expected a regular download file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

pub(crate) fn reset_part(path: &Path, total: Option<u64>) -> Result<()> {
    let file = if regular_file_length(path)?.is_some() {
        OpenOptions::new().write(true).truncate(true).open(path)?
    } else {
        OpenOptions::new().write(true).create_new(true).open(path)?
    };
    file.set_len(total.unwrap_or(0))?;
    file.sync_all()?;
    Ok(())
}

pub(crate) async fn sync_part(path: &Path) -> Result<()> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await?
        .sync_all()
        .await?;
    Ok(())
}

pub(crate) struct Verification {
    pub digest: String,
    pub sha256_verified: Option<bool>,
    pub mismatch: Option<&'static str>,
}

fn file_hashes(
    path: &Path,
    want_md5: bool,
    check: impl Fn() -> Result<()>,
) -> Result<(String, Option<String>)> {
    let mut file = File::open(path)?;
    let mut sha = Sha256::new();
    let mut md5 = want_md5.then(Md5::new);
    let mut buffer = vec![0; 256 * 1024];
    loop {
        check()?;
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        sha.update(&buffer[..len]);
        if let Some(md5) = &mut md5 {
            md5.update(&buffer[..len]);
        }
    }
    Ok((
        hex::encode(sha.finalize()),
        md5.map(|md5| hex::encode(md5.finalize())),
    ))
}

pub(crate) fn prepare_commit(
    record: &Record,
    check: impl Fn() -> Result<()>,
) -> Result<Verification> {
    let (part, _) = record.paths()?;
    if regular_file_length(&part)? != record.total || record.total.is_none() {
        return Err(EngineError::InvalidState(
            "part file length differs from download size",
        ));
    }
    OpenOptions::new().write(true).open(&part)?.sync_all()?;
    let (digest, md5) = file_hashes(&part, record.expected_md5.is_some(), &check)?;
    let sha256_verified = record
        .expected_sha256
        .as_ref()
        .map(|expected| *expected == digest);
    let mismatch = if sha256_verified == Some(false) {
        Some("SHA-256")
    } else if record.expected_md5 != md5 {
        Some("MD5")
    } else {
        None
    };
    if mismatch.is_none() {
        check()?;
        mark_of_the_web(&part, &record.source_url, record.referrer.as_deref())?;
    }
    Ok(Verification {
        digest,
        sha256_verified,
        mismatch,
    })
}

pub(crate) fn recovered_commit(record: &Record) -> Result<bool> {
    let Some(digest) = &record.commit_digest else {
        return Ok(false);
    };
    let (part, final_path) = record.paths()?;
    let Some(total) = record.total else {
        return Ok(false);
    };
    match regular_file_length(&final_path) {
        Ok(Some(length)) if length == total => {}
        Ok(_) | Err(EngineError::InvalidState(_)) => return Ok(false),
        Err(error) => return Err(error),
    }
    let part_exists = regular_file_length(&part)?.is_some();
    if part_exists && !same_file(&part, &final_path)? {
        return Ok(false);
    }
    if file_hashes(&final_path, false, || Ok(()))?.0 != *digest {
        return Ok(false);
    }
    if part_exists {
        fs::remove_file(part)?;
    }
    Ok(true)
}

#[cfg(unix)]
fn same_file(part: &Path, final_path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let part = fs::metadata(part)?;
    let final_path = fs::metadata(final_path)?;
    Ok(part.dev() == final_path.dev() && part.ino() == final_path.ino())
}

#[cfg(not(unix))]
fn same_file(_part: &Path, _final_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

pub(crate) async fn commit(record: &Record) -> Result<()> {
    let (part, final_path) = record.paths()?;
    let overwrite = record.overwrite_existing;
    let directory = record.save_dir.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        let _directory_handle = overwrite
            .then(|| hold_commit_directory(&directory))
            .transpose()?;
        if overwrite {
            if final_path.parent() != Some(directory.as_path())
                || fs::canonicalize(&directory)? != directory
            {
                return Err(EngineError::InvalidState("download directory changed"));
            }
            for ancestor in directory.ancestors().filter(|path| path.is_absolute()) {
                let metadata = fs::symlink_metadata(ancestor)?;
                if !metadata.is_dir() || is_reparse_point(&metadata) {
                    return Err(EngineError::InvalidState("unsafe download directory"));
                }
            }
            if regular_file_length(&part)?.is_none() {
                return Err(EngineError::InvalidState("missing download part file"));
            }
        }
        move_file(&part, &final_path, overwrite)
    })
    .await?
}

#[cfg(windows)]
fn hold_commit_directory(directory: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    // Keep the destination directory open without write or delete sharing until the move.
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(directory)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || is_reparse_point(&metadata) {
        return Err(EngineError::InvalidState("unsafe download directory"));
    }
    Ok(file)
}

pub fn numbered_filename(filename: &str, number: usize) -> String {
    if number == 0 {
        return filename.to_owned();
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(filename);
    let suffix = format!(" ({number})");
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        let available = MAX_FILENAME_BYTES - suffix.len() - 1;
        let first_char = stem.chars().next().map_or(0, char::len_utf8);
        let extension = &extension[..extension.floor_char_boundary(available - first_char)];
        let extension = extension.trim_end_matches(['.', ' ']);
        if !extension.is_empty() {
            let stem = &stem[..stem.floor_char_boundary(available - extension.len())];
            return format!("{stem}{suffix}.{extension}");
        }
    }
    let stem = &stem[..stem.floor_char_boundary(MAX_FILENAME_BYTES - suffix.len())];
    format!("{stem}{suffix}")
}

#[cfg(windows)]
fn move_file(part: &Path, final_path: &Path, overwrite: bool) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const ERROR_SHARING_VIOLATION: i32 = 32;

    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let part = wide(part);
    let destination = wide(final_path);
    let flags = if overwrite {
        MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING
    } else {
        MOVEFILE_WRITE_THROUGH
    };
    // Both owned, NUL-terminated strings live across the call. The source and
    // destination share a directory and the flags forbid copy.
    for attempt in 0..5 {
        if overwrite {
            regular_file_length(final_path)?;
        }
        if unsafe { MoveFileExW(part.as_ptr(), destination.as_ptr(), flags) } != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) && attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        if !overwrite
            && error.kind() == std::io::ErrorKind::PermissionDenied
            && fs::symlink_metadata(final_path).is_ok()
        {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into());
        }
        return Err(error.into());
    }
    unreachable!("move_file loop always returns on final attempt")
}

#[cfg(not(windows))]
fn move_file(part: &Path, final_path: &Path, overwrite: bool) -> Result<()> {
    if overwrite {
        regular_file_length(final_path)?;
        fs::rename(part, final_path)?;
    } else {
        fs::hard_link(part, final_path)?;
        fs::remove_file(part)?;
    }
    File::open(final_path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn mark_of_the_web(path: &Path, host: &str, referrer: Option<&str>) -> Result<()> {
    let mut stream = path.as_os_str().to_os_string();
    stream.push(":Zone.Identifier");
    let mut file = File::create(stream)?;
    let clean = |url: &str| {
        crate::http::sanitize_http_url_credentials(url)
            .unwrap_or_else(|| url.replace(['\r', '\n', '\0'], ""))
    };
    write!(file, "[ZoneTransfer]\r\nZoneId=3\r\n")?;
    if let Some(referrer) = referrer.and_then(crate::http::sanitize_http_url_credentials) {
        write!(file, "ReferrerUrl={referrer}\r\n")?;
    }
    write!(file, "HostUrl={}\r\n", clean(host))?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn mark_of_the_web(_path: &Path, _host: &str, _referrer: Option<&str>) -> Result<()> {
    Ok(())
}
