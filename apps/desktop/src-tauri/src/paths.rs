use slipstream_protocol::{safe_join_under, validate_safe_filename_component};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem::size_of,
    os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf, Prefix},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Security::Cryptography::{
        BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash, BCryptHashData, BCRYPT_HASH_HANDLE,
        BCRYPT_SHA256_ALG_HANDLE,
    },
    Storage::FileSystem::{
        FileDispositionInfo, GetFinalPathNameByHandleW, SetFileInformationByHandle, DELETE,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(crate) fn local_drive_path(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let mut components = path.components();
    let drive = match (components.next(), components.next()) {
        (Some(Component::Prefix(prefix)), Some(Component::RootDir)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(invalid("expected a local drive path")),
        },
        _ => return Err(invalid("expected an absolute local drive path")),
    };
    let mut clean = PathBuf::from(format!("{}:\\", char::from(drive)));
    for part in components {
        let Component::Normal(part) = part else {
            return Err(invalid("expected a canonical local drive path"));
        };
        let name = part
            .to_str()
            .ok_or_else(|| invalid("expected a Unicode local drive path"))?;
        validate_safe_filename_component(name).map_err(|error| invalid(error.to_string()))?;
        clean.push(part);
    }
    Ok(clean)
}

pub(crate) fn validate_existing_ancestors(clean: &Path) -> io::Result<()> {
    let ancestors: Vec<_> = clean
        .ancestors()
        .take_while(|path| path.is_absolute())
        .collect();
    for ancestor in ancestors.into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if !metadata.is_dir() || reparse(&metadata) {
                    return Err(invalid(
                        "directory contains a non-directory or reparse point",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn ensure_safe_directory(root: &Path) -> io::Result<PathBuf> {
    if !root.is_absolute() {
        return Err(invalid("expected an absolute local directory path"));
    }
    let clean = local_drive_path(root)?;
    validate_existing_ancestors(&clean)?;
    fs::create_dir_all(&clean)?;
    canonical_directory(&clean)
}

pub(crate) fn canonical_directory(root: &Path) -> io::Result<PathBuf> {
    if !root.is_absolute() {
        return Err(invalid("expected an absolute local directory path"));
    }
    local_drive_path(root)?;
    let ancestors: Vec<_> = root
        .ancestors()
        .take_while(|path| path.is_absolute())
        .collect();
    for directory in ancestors.into_iter().rev() {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || reparse(&metadata) {
            return Err(invalid(
                "save directory contains a non-directory or reparse point",
            ));
        }
    }
    let canonical = fs::canonicalize(root)?;
    local_drive_path(&canonical)?;
    if matches!(root.components().next(), Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_)))
        && canonical != root
    {
        return Err(invalid("save directory changed since it was resolved"));
    }
    Ok(canonical)
}

/// Walk each component before canonicalizing so even a dangling junction is
/// rejected, and a missing leaf can be distinguished from an unsafe path.
pub(crate) fn confined_target(root: &Path, relative: &str) -> io::Result<Option<PathBuf>> {
    let canonical_root = canonical_directory(root)?;
    let candidate =
        safe_join_under(&canonical_root, relative).map_err(|e| invalid(e.to_string()))?;
    let suffix = candidate
        .strip_prefix(&canonical_root)
        .map_err(|_| invalid("path escapes the save directory"))?;
    let mut current = canonical_root.clone();
    for part in suffix.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if reparse(&metadata) => {
                return Err(invalid("path contains a reparse point"))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    let canonical = fs::canonicalize(current)?;
    if !canonical.starts_with(&canonical_root) || canonical == canonical_root {
        return Err(invalid("path escapes the save directory"));
    }
    Ok(Some(canonical))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevealAction {
    SelectFile(OsString),
    OpenDir(PathBuf),
}

pub(crate) fn reveal_action(root: &Path, relative: Option<&str>) -> io::Result<RevealAction> {
    if let Some(relative) = relative {
        if let Some(target) = confined_target(root, relative)? {
            let drive = local_drive_path(&target)?;
            let text = drive
                .to_str()
                .ok_or_else(|| invalid("expected a Unicode local drive path"))?;
            if text.contains('"') || text.chars().any(char::is_control) {
                return Err(invalid("invalid characters in reveal path"));
            }
            return Ok(RevealAction::SelectFile(OsString::from(format!(
                "/select,\"{text}\""
            ))));
        }
    }
    Ok(RevealAction::OpenDir(local_drive_path(
        &canonical_directory(root)?,
    )?))
}

pub(crate) fn build_defender_scan_args(root: &Path, relative: &str) -> io::Result<Vec<OsString>> {
    let path = existing_file(root, relative)?;
    let mut args: Vec<OsString> = ["-Scan", "-ScanType", "3", "-File"]
        .map(OsString::from)
        .into();
    args.push(local_drive_path(&path)?.into_os_string());
    Ok(args)
}

pub(crate) fn canonical_exe(path: &Path) -> io::Result<PathBuf> {
    let has_exe_extension = |path: &Path| {
        path.extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    };
    if !path.is_absolute() {
        return Err(invalid("expected an absolute .exe path"));
    }
    let clean = local_drive_path(path)?;
    if !has_exe_extension(&clean) {
        return Err(invalid("expected an .exe file"));
    }
    if let Some(parent) = clean.parent() {
        canonical_directory(parent)?;
    }
    let metadata = fs::symlink_metadata(&clean)?;
    if !metadata.is_file() || reparse(&metadata) {
        return Err(invalid("expected a regular .exe file"));
    }
    let canonical = fs::canonicalize(&clean)?;
    local_drive_path(&canonical)?;
    if !has_exe_extension(&canonical) {
        return Err(invalid("expected an .exe file"));
    }
    Ok(canonical)
}

pub(crate) fn validate_configured_exe(raw: &str) -> io::Result<PathBuf> {
    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(invalid(
            "executable path contains invalid control characters",
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(invalid("expected an absolute local .exe path"));
    }
    let clean = local_drive_path(path)?;
    let is_exe = clean
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
    if !is_exe {
        return Err(invalid("expected an .exe file"));
    }
    let parent = clean
        .parent()
        .ok_or_else(|| invalid("executable path needs a parent directory"))?;
    validate_existing_ancestors(parent)?;
    match fs::symlink_metadata(&clean) {
        Ok(_) => {
            canonical_exe(&clean)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(clean)
}

fn existing_file(root: &Path, relative: &str) -> io::Result<PathBuf> {
    let path = confined_target(root, relative)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "transfer file is missing"))?;
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || reparse(&metadata) {
        return Err(invalid("expected a regular transfer file"));
    }
    Ok(path)
}

fn file_for_operation(path: &Path, delete: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    if delete {
        options.access_mode(DELETE | FILE_READ_ATTRIBUTES);
    } else {
        options.read(true);
    }
    let file = options
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if path.as_os_str() != opened_file_path(&file)? {
        return Err(invalid("transfer path changed during open"));
    }
    Ok(file)
}

fn opened_file_path(file: &File) -> io::Result<OsString> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || reparse(&metadata) {
        return Err(invalid("expected a regular non-reparse file"));
    }
    // Check the path of the opened handle to catch a parent replaced between
    // validation and open. Deletion and hashing both use this same handle.
    let handle = file.as_raw_handle();
    let needed = unsafe { GetFinalPathNameByHandleW(handle, null_mut(), 0, 0) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0; needed as usize];
    let written = unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), needed, 0) };
    if written == 0 || written >= needed {
        return Err(io::Error::last_os_error());
    }
    Ok(OsString::from_wide(&buffer[..written as usize]))
}

#[cfg(windows)]
pub(crate) fn write_zone_identifier(root: &Path, relative: &str) -> io::Result<()> {
    let path = existing_file(root, relative)?;
    let base = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
    if path.as_os_str() != opened_file_path(&base)? {
        return Err(invalid("transfer path changed during open"));
    }
    let mut stream = path.into_os_string();
    stream.push(":Zone.Identifier");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&stream)?;
    let opened = opened_file_path(&file)?;
    if opened != stream {
        stream.push(":$DATA");
        if opened != stream {
            return Err(invalid("transfer stream path changed during open"));
        }
    }
    file.set_len(0)?;
    file.write_all(b"[ZoneTransfer]\r\nZoneId=3\r\n")
}

pub(crate) fn delete_files(root: &Path, paths: &[String]) -> io::Result<()> {
    let mut handles = Vec::new();
    for relative in paths {
        match existing_file(root, relative) {
            Ok(path) => handles.push(file_for_operation(&path, true)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    for file in handles {
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&raw const disposition).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn decode_sha256(expected: &str) -> io::Result<[u8; 32]> {
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("expected exactly 64 hexadecimal SHA-256 digits"));
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&expected[index * 2..index * 2 + 2], 16)
            .map_err(|error| invalid(error.to_string()))?;
    }
    Ok(bytes)
}

pub(crate) fn hash_file(root: &Path, relative: &str) -> io::Result<[u8; 32]> {
    let path = existing_file(root, relative)?;
    let mut file = file_for_operation(&path, false)?;
    struct Hash(BCRYPT_HASH_HANDLE);
    impl Drop for Hash {
        fn drop(&mut self) {
            unsafe { BCryptDestroyHash(self.0) };
        }
    }
    fn check(status: i32) -> io::Result<()> {
        if status < 0 {
            Err(io::Error::other(format!(
                "Windows SHA-256 error {status:#x}"
            )))
        } else {
            Ok(())
        }
    }
    let mut handle = null_mut();
    check(unsafe {
        BCryptCreateHash(
            BCRYPT_SHA256_ALG_HANDLE,
            &mut handle,
            null_mut(),
            0,
            null(),
            0,
            0,
        )
    })?;
    let hash = Hash(handle);
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        check(unsafe { BCryptHashData(hash.0, buffer.as_ptr(), count as u32, 0) })?;
    }
    let mut digest = [0u8; 32];
    check(unsafe { BCryptFinishHash(hash.0, digest.as_mut_ptr(), digest.len() as u32, 0) })?;
    Ok(digest)
}
