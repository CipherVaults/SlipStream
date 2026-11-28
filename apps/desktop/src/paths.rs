use slipstream_media::{args::validate_local_path, MediaError};
use slipstream_protocol::{
    safe_join_under, sanitize_title_to_filename, validate_safe_filename_component,
    PathSecurityError, TransferStatus,
};
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
        FileDispositionInfo, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(crate) fn local_drive_path(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let raw = path
        .as_os_str()
        .to_str()
        .ok_or_else(|| invalid("expected a Unicode local drive path"))?;
    if raw.chars().any(char::is_control)
        || raw
            .split(['/', '\\'])
            .any(|part| matches!(part, "." | ".."))
    {
        return Err(invalid("path contains traversal or control characters"));
    }
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

pub(crate) fn ensure_safe_tree(root: &Path) -> io::Result<PathBuf> {
    const MAX_DEPTH: usize = 32;
    const MAX_ENTRIES: usize = 16_384;
    let root = ensure_safe_directory(root)?;
    let mut directories = vec![(root.clone(), 0)];
    let mut count = 0;
    while let Some((directory, depth)) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            count += 1;
            if count > MAX_ENTRIES || depth >= MAX_DEPTH {
                return Err(invalid("cache directory tree exceeds inspection limits"));
            }
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if reparse(&metadata) || !(metadata.is_file() || metadata.is_dir()) {
                return Err(invalid(
                    "cache directory contains a reparse point or special file",
                ));
            }
            if metadata.is_dir() {
                directories.push((path, depth + 1));
            }
        }
    }
    Ok(root)
}

pub(crate) fn media_title_stem(title: &str, fallback: &str) -> String {
    let name = sanitize_title_to_filename(title, "txt").unwrap_or_default();
    let stem = name.strip_suffix(".txt").unwrap_or(&name);
    let mut end = stem.len().min(120);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(['.', ' ']);
    if stem.is_empty() || stem.eq_ignore_ascii_case("download") {
        fallback.to_owned()
    } else {
        stem.to_owned()
    }
}

pub(crate) fn ensure_playlist_subfolder(base_dir: &Path, raw_name: &str) -> io::Result<PathBuf> {
    if Path::new(raw_name.trim()).has_root()
        || raw_name
            .split(['/', '\\'])
            .any(|part| matches!(part.trim(), "." | ".."))
    {
        return Err(invalid("playlist title contains path traversal"));
    }
    let raw_stem = raw_name.split('.').next().unwrap_or_default().trim();
    if matches!(
        validate_local_path(Path::new(raw_stem)),
        Err(MediaError::Path(PathSecurityError::ReservedDeviceName))
    ) {
        return Err(invalid("playlist title is a Windows reserved device name"));
    }
    let base = canonical_directory(base_dir)?;
    let stem = media_title_stem(raw_name, "Playlist");
    for number in 1..=1000 {
        let name = if number == 1 {
            stem.clone()
        } else {
            format!("{stem} ({number})")
        };
        validate_local_path(Path::new(&name)).map_err(|error| invalid(error.to_string()))?;
        let target = safe_join_under(&base, &name).map_err(|error| invalid(error.to_string()))?;
        match fs::symlink_metadata(&target) {
            Ok(metadata) if reparse(&metadata) => {
                return Err(invalid("playlist folder is a reparse point"))
            }
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match fs::create_dir(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
        let directory = canonical_directory(&target)?;
        if directory.parent() != Some(base.as_path()) {
            return Err(invalid("playlist folder is outside the save directory"));
        }
        return Ok(directory);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no available playlist folder name",
    ))
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

pub(crate) fn with_completed_file<T>(
    status: TransferStatus,
    root: &Path,
    relative: &str,
    open: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<T> {
    if status != TransferStatus::Completed {
        return Err(invalid("only completed files can be opened"));
    }
    let path = existing_file(root, relative)?;
    // Hold the checked file without write or delete sharing until the shell receives the path.
    let _file = file_for_operation(&path, false)?;
    open(&local_drive_path(&path)?)
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
    final_handle_path(file)
}

pub(crate) fn final_handle_path(file: &File) -> io::Result<OsString> {
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

pub(crate) fn verify_handle_path(file: &File, expected: &Path) -> io::Result<PathBuf> {
    let actual = local_drive_path(Path::new(&final_handle_path(file)?))?;
    let expected = local_drive_path(expected)?;
    if !actual
        .as_os_str()
        .eq_ignore_ascii_case(expected.as_os_str())
    {
        return Err(invalid("path changed during open"));
    }
    Ok(actual)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity(u32, u64);

pub(crate) fn file_identity(file: &File) -> io::Result<(FileIdentity, u32)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        FileIdentity(
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ),
        info.nNumberOfLinks,
    ))
}

pub(crate) fn open_local_file(path: &Path, delete_access: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    if delete_access {
        options.access_mode(windows_sys::Win32::Foundation::GENERIC_READ | DELETE);
    } else {
        options.read(true);
    }
    let file = options
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || reparse(&metadata) {
        return Err(invalid("expected a regular non-reparse file"));
    }
    verify_handle_path(&file, path)?;
    Ok(file)
}

pub(crate) fn reject_existing_links(path: &Path) -> io::Result<PathBuf> {
    let path = local_drive_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("expected a file path"))?;
    validate_existing_ancestors(parent)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() || reparse(&metadata) => {
            return Err(invalid("expected a regular non-reparse file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(path)
}

pub(crate) fn open_unlinked_local_file(path: &Path) -> io::Result<File> {
    let file = open_local_file(path, false)?;
    if file_identity(&file)?.1 != 1 {
        return Err(invalid("expected a file without hard links"));
    }
    Ok(file)
}

pub(crate) fn open_local_directory(path: &Path) -> io::Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        // Keep the directory from being renamed while allowing child
        // creation and the filesystem's destination open during publication.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let metadata = directory.metadata()?;
    if reparse(&metadata) || !metadata.is_dir() {
        return Err(invalid("expected a regular non-reparse directory"));
    }
    verify_handle_path(&directory, path)?;
    Ok(directory)
}

pub(crate) fn delete_open_file(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    for file in handles {
        delete_open_file(&file)?;
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
