use crate::paths::{
    canonical_directory, delete_open_file, file_identity, final_handle_path, invalid,
    local_drive_path, open_local_directory, open_local_file, reparse, verify_handle_path,
    FileIdentity,
};
use flate2::read::MultiGzDecoder;
use sevenz_rust::{Password, SevenZReader};
use slipstream_protocol::{
    sanitize_filename, validate_safe_filename_component, AutoExtractSettings,
};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, Write};
use std::mem::{offset_of, size_of};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::path::{Path, PathBuf};
use tar::{Archive, EntryType};
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        FileRenameInfo, FileRenameInfoEx, SetFileInformationByHandle, DELETE,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ,
    },
};
use zip::{unstable::read::ZipIterable, ZipArchive};

const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_ARCHIVE_DEPTH: usize = 32;
const MAX_TOTAL_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionSummary {
    pub destination_dir: PathBuf,
    pub files_extracted: usize,
    pub skipped_existing: usize,
    pub total_bytes: u64,
    pub deleted_archive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
    SevenZ,
}

fn detect_archive_format(path: &Path) -> Option<ArchiveFormat> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar") {
        Some(ArchiveFormat::Tar)
    } else if name.ends_with(".zip") {
        Some(ArchiveFormat::Zip)
    } else if name.ends_with(".7z") {
        Some(ArchiveFormat::SevenZ)
    } else {
        None
    }
}

/// Returns true when the path uses an archive format supported by SlipStream's
/// pure-Rust extractor (`.zip`, `.tar`, `.tar.gz`, `.tgz`, `.7z`).
pub fn is_supported_archive(path: &Path) -> bool {
    detect_archive_format(path).is_some()
}

fn archive_subfolder_name(archive_path: &Path) -> String {
    let raw_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("extracted");
    let lower = raw_name.to_ascii_lowercase();
    let stem = if lower.ends_with(".tar.gz") && raw_name.len() > ".tar.gz".len() {
        &raw_name[..raw_name.len() - ".tar.gz".len()]
    } else {
        archive_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("extracted")
    };
    let clean = sanitize_filename(stem).unwrap_or_else(|_| "extracted".to_owned());
    if validate_safe_filename_component(&clean).is_ok() {
        clean
    } else {
        "extracted".to_owned()
    }
}

fn validate_entry_components(raw: &str, is_dir: bool) -> Result<Option<Vec<String>>, String> {
    let normalized = raw.replace('\\', "/");
    let mut trimmed = normalized.as_str();
    if is_dir && matches!(trimmed, "" | "." | "./") {
        return Ok(None);
    }
    if trimmed.starts_with('/') {
        return Err(format!("archive entry uses an absolute path: {raw}"));
    }
    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest;
    }
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() {
        if is_dir {
            return Ok(None);
        }
        return Err("archive entry has an empty path".to_owned());
    }
    if trimmed.as_bytes().get(1) == Some(&b':')
        && trimmed
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
    {
        return Err(format!("archive entry specifies a drive prefix: {raw}"));
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() > MAX_ARCHIVE_DEPTH {
        return Err(format!("archive entry exceeds depth limit: {raw}"));
    }
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        validate_safe_filename_component(part)
            .map_err(|error| format!("unsafe archive entry '{raw}': {error}"))?;
        out.push(part.to_owned());
    }
    Ok(Some(out))
}

struct ExtractionTarget {
    canonical_root: PathBuf,
    source: FileIdentity,
}

impl ExtractionTarget {
    fn verify(&self, file: &File, expected: &Path) -> io::Result<()> {
        let actual = verify_handle_path(file, expected)?;
        if !actual.starts_with(&self.canonical_root) {
            return Err(invalid("archive target escaped extraction root"));
        }
        verify_single_link(file, Some(self.source))?;
        Ok(())
    }

    fn ensure_safe_subdir(&self, components: &[String]) -> io::Result<(PathBuf, Vec<File>)> {
        let mut current = self.canonical_root.clone();
        let mut parents = Vec::new();
        for component in components {
            current.push(component);
            let directory = open_or_create_directory(&current)?;
            self.verify(&directory, &current)?;
            // Keep every ancestor open through publication to prevent directory renames.
            parents.push(directory);
        }
        Ok((current, parents))
    }
}

fn open_or_create_directory(path: &Path) -> io::Result<File> {
    match open_local_directory(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            open_local_directory(path)
        }
        result => result,
    }
}

fn verify_single_link(file: &File, source: Option<FileIdentity>) -> io::Result<FileIdentity> {
    let (identity, links) = file_identity(file)?;
    if Some(identity) == source || links != 1 {
        return Err(invalid(
            "archive path is a source alias or has multiple hard links",
        ));
    }
    Ok(identity)
}

fn verify_regular(file: &File) -> io::Result<()> {
    let meta = file.metadata()?;
    if reparse(&meta) || !meta.is_file() {
        return Err(invalid("archive path is a reparse point or non-file"));
    }
    Ok(())
}

struct StagedFile {
    file: File,
    published: bool,
}

impl StagedFile {
    fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .create_new(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        Ok(Self {
            file,
            published: false,
        })
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = delete_open_file(&self.file);
        }
    }
}

const ZONE_IDENTIFIER: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\n";

fn stage_zone_identifier(path: &Path, base: &File) -> io::Result<File> {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    let mut ads = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&stream)?;
    verify_regular(&ads)?;
    let opened = final_handle_path(&ads)?;
    let mut expected = final_handle_path(base)?;
    expected.push(":Zone.Identifier");
    if opened != expected {
        expected.push(":$DATA");
        if opened != expected {
            return Err(invalid("staged Mark-of-the-Web path changed during open"));
        }
    }
    if file_identity(&ads)? != file_identity(base)? {
        return Err(invalid(
            "staged Mark-of-the-Web belongs to a different file",
        ));
    }
    ads.write_all(ZONE_IDENTIFIER)?;
    ads.sync_all()?;
    ads.rewind()?;
    let mut contents = Vec::new();
    (&mut ads)
        .take(ZONE_IDENTIFIER.len() as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents != ZONE_IDENTIFIER {
        return Err(invalid("staged Mark-of-the-Web verification failed"));
    }
    Ok(ads)
}

fn publish(file: &File, target: &Path, replace: bool) -> io::Result<()> {
    const FILE_RENAME_REPLACE_IF_EXISTS: u32 = 1;
    const FILE_RENAME_POSIX_SEMANTICS: u32 = 2;
    let name: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let name_bytes = std::mem::size_of_val(name.as_slice());
    let offset = offset_of!(FILE_RENAME_INFO, FileName);
    let length = (offset + name_bytes).max(size_of::<FILE_RENAME_INFO>());
    // The structure ends with a variable-size UTF-16 path. Allocate aligned,
    // zeroed storage for both the fixed header and the complete NUL-terminated name.
    let mut buffer = vec![0_usize; length.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let class = if replace {
        FileRenameInfoEx
    } else {
        FileRenameInfo
    };
    unsafe {
        if replace {
            (*info).Anonymous.Flags = FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
        }
        (*info).FileNameLength = (name_bytes - size_of::<u16>()) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<u8>().add(offset).cast(),
            name.len(),
        );
        if SetFileInformationByHandle(file.as_raw_handle(), class, info.cast(), length as u32) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[derive(Default)]
struct ExtractState {
    entries_seen: usize,
    files_extracted: usize,
    skipped_existing: usize,
    total_bytes: u64,
    bytes_consumed: u64,
}

impl ExtractState {
    fn count_entry(&mut self) -> Result<(), String> {
        self.entries_seen += 1;
        if self.entries_seen > MAX_ARCHIVE_ENTRIES {
            return Err(format!(
                "archive exceeds maximum entry limit ({MAX_ARCHIVE_ENTRIES})"
            ));
        }
        Ok(())
    }

    fn remaining_budget(&self) -> u64 {
        MAX_TOTAL_EXTRACTED_BYTES.saturating_sub(self.bytes_consumed)
    }

    fn check_declared_size(&self, size: u64) -> Result<(), String> {
        if size > self.remaining_budget() {
            return Err("archive exceeds maximum allowed uncompressed size (4 GiB)".to_owned());
        }
        Ok(())
    }
}

fn write_file_entry(
    target: &ExtractionTarget,
    components: &[String],
    reader: &mut dyn Read,
    declared_size: u64,
    overwrite: bool,
    state: &mut ExtractState,
) -> Result<(), String> {
    write_entry(target, components, reader, declared_size, overwrite, state)
        .map_err(|error| error.to_string())
}

fn write_entry(
    target: &ExtractionTarget,
    components: &[String],
    reader: &mut dyn Read,
    declared_size: u64,
    overwrite: bool,
    state: &mut ExtractState,
) -> io::Result<()> {
    state.check_declared_size(declared_size).map_err(invalid)?;
    let (file_name, parent_parts) = components
        .split_last()
        .ok_or_else(|| invalid("missing file component"))?;
    let (parent, _parents) = target.ensure_safe_subdir(parent_parts)?;
    let destination = parent.join(file_name);
    let existing = match fs::symlink_metadata(&destination) {
        Ok(meta) => {
            if reparse(&meta) || !meta.is_file() {
                return Err(invalid("archive target is a reparse point or non-file"));
            }
            let file = OpenOptions::new()
                .read(true)
                // NTFS requires delete sharing on a target held open across replacement.
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&destination)?;
            verify_regular(&file)?;
            target.verify(&file, &destination)?;
            Some(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let budget = state.remaining_budget();
    let mut limited = reader.take(budget + 1);
    if existing.is_some() && !overwrite {
        let drained = io::copy(&mut limited, &mut io::sink())?;
        if drained != declared_size {
            return Err(invalid("archive entry disagrees with its declared size"));
        }
        state.bytes_consumed += drained;
        state.skipped_existing += 1;
        return Ok(());
    }

    let path = parent.join(format!(".slipstream-tmp-{}", Uuid::new_v4()));
    let mut stage = StagedFile::create(&path)?;
    verify_regular(&stage.file)?;
    target.verify(&stage.file, &path)?;
    let written = io::copy(&mut limited, &mut stage.file)?;
    if written != declared_size {
        return Err(invalid("archive entry disagrees with its declared size"));
    }
    stage.file.sync_all()?;
    let _zone = stage_zone_identifier(&path, &stage.file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not stage Mark-of-the-Web: {error}"),
        )
    })?;
    target.verify(&stage.file, &path)?;
    if let Some(file) = &existing {
        verify_regular(file)?;
        target.verify(file, &destination)?;
    }
    // Replace only a vetted occupant. When the destination was absent, use
    // a kernel-enforced no-replace rename even if overwrite was requested.
    publish(&stage.file, &destination, existing.is_some()).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not publish '{}': {error}", destination.display()),
        )
    })?;
    stage.published = true;
    state.bytes_consumed += written;
    state.total_bytes += written;
    state.files_extracted += 1;
    Ok(())
}

fn extract_zip(
    file: &File,
    target: &ExtractionTarget,
    overwrite: bool,
    state: &mut ExtractState,
) -> Result<(), String> {
    let raw = ZipIterable::try_new(BufReader::new(file), Default::default())
        .map_err(|error| error.to_string())?;
    if raw.len() > MAX_ARCHIVE_ENTRIES {
        return Err(format!(
            "archive exceeds maximum entry limit ({MAX_ARCHIVE_ENTRIES})"
        ));
    }
    let mut archive = ZipArchive::new(BufReader::new(file)).map_err(|error| error.to_string())?;
    if archive.has_duplicate_names() || archive.len() != raw.len() {
        return Err("ZIP contains duplicate entry names or inconsistent entry counts".into());
    }
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry
            .name()
            .map_err(|error| format!("invalid ZIP entry name: {error:?}"))?;
        if !names.insert(name.into_owned()) {
            return Err("ZIP contains duplicate entry names".into());
        }
    }
    for index in 0..archive.len() {
        state.count_entry()?;
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let entry_name = entry
            .name()
            .map_err(|error| format!("invalid ZIP entry name: {error:?}"))?;
        let unix_type = entry.unix_mode().unwrap_or_default() & 0o170_000;
        if entry.is_symlink() || !matches!(unix_type, 0 | 0o100_000 | 0o040_000) {
            return Err(format!(
                "archive entry is a symbolic link or special file: {entry_name}"
            ));
        }
        let is_dir = entry.is_dir();
        let size = entry.size();
        if is_dir && size != 0 {
            return Err("ZIP directory entry contains data".into());
        }
        if let Some(components) = validate_entry_components(&entry_name, is_dir)? {
            if is_dir {
                target
                    .ensure_safe_subdir(&components)
                    .map_err(|error| error.to_string())?;
            } else {
                write_file_entry(target, &components, &mut entry, size, overwrite, state)?;
            }
        }
    }
    Ok(())
}

const MAX_TAR_STREAM_BYTES: u64 = MAX_TOTAL_EXTRACTED_BYTES + 32 * 1024 * 1024;

fn extract_tar(
    reader: impl Read,
    target: &ExtractionTarget,
    overwrite: bool,
    state: &mut ExtractState,
) -> Result<(), String> {
    let mut limited = reader.take(MAX_TAR_STREAM_BYTES);
    let mut archive = Archive::new(&mut limited);
    for entry in archive.entries().map_err(|error| error.to_string())? {
        state.count_entry()?;
        let mut entry = entry.map_err(|error| error.to_string())?;
        let entry_type = entry.header().entry_type();
        if !matches!(
            entry_type,
            EntryType::Regular | EntryType::Continuous | EntryType::Directory
        ) {
            return Err(format!("unsupported tar entry type: {entry_type:?}"));
        }
        let is_dir = entry_type == EntryType::Directory;
        let size = entry.size();
        if is_dir && size != 0 {
            return Err("tar directory entry contains data".into());
        }
        let path = entry.path().map_err(|error| error.to_string())?;
        if let Some(components) = validate_entry_components(&path.to_string_lossy(), is_dir)? {
            if is_dir {
                target
                    .ensure_safe_subdir(&components)
                    .map_err(|error| error.to_string())?;
            } else {
                write_file_entry(target, &components, &mut entry, size, overwrite, state)?;
            }
        }
    }
    // TAR stops at its end marker. Read the remaining padding and force the
    // gzip decoder through the trailer so CRC and uncompressed length are checked.
    io::copy(archive.into_inner(), &mut io::sink()).map_err(|error| error.to_string())?;
    if limited.limit() == 0 {
        return Err("tar exceeds maximum decompressed stream size".into());
    }
    Ok(())
}

fn extract_7z(
    file: &File,
    length: u64,
    target: &ExtractionTarget,
    overwrite: bool,
    state: &mut ExtractState,
) -> Result<(), String> {
    let mut reader = SevenZReader::new(BufReader::new(file), length, Password::empty())
        .map_err(|error| error.to_string())?;
    let mut inner_error = None;
    let status = reader.for_each_entries(|entry, stream| {
        let result = (|| {
            state.count_entry()?;
            let posix_type = (entry.windows_attributes >> 16) & 0o170_000;
            if entry.is_anti_item
                || (entry.windows_attributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0
                || !matches!(posix_type, 0 | 0o100_000 | 0o040_000)
            {
                return Err(format!("unsupported 7z entry attributes: {}", entry.name()));
            }
            let is_dir = entry.is_directory();
            if is_dir && (entry.size != 0 || entry.has_stream) {
                return Err(format!(
                    "7z directory entry contains data: {}",
                    entry.name()
                ));
            }
            if let Some(components) = validate_entry_components(entry.name(), is_dir)? {
                if is_dir {
                    target
                        .ensure_safe_subdir(&components)
                        .map_err(|error| error.to_string())?;
                } else {
                    write_file_entry(target, &components, stream, entry.size, overwrite, state)?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(true),
            Err(error) => {
                inner_error = Some(error);
                Ok(false)
            }
        }
    });
    inner_error.map_or_else(|| status.map_err(|error| error.to_string()), Err)
}

/// Safely extracts a supported archive on a local drive using pure-Rust decoders,
/// enforcing path traversal rejection, symlink/hardlink rejection, entry & byte caps,
/// and Mark-of-the-Web (`ZoneId=3`) propagation.
pub fn extract_archive(
    archive_path: &Path,
    settings: &AutoExtractSettings,
) -> Result<ExtractionSummary, String> {
    extract_archive_after_open(archive_path, settings, || {})
}

pub(crate) fn extract_archive_after_open(
    archive_path: &Path,
    settings: &AutoExtractSettings,
    after_open: impl FnOnce(),
) -> Result<ExtractionSummary, String> {
    let format = detect_archive_format(archive_path)
        .ok_or_else(|| format!("unsupported archive format: {}", archive_path.display()))?;
    let clean_archive = local_drive_path(archive_path).map_err(|error| error.to_string())?;
    let parent = clean_archive
        .parent()
        .ok_or("archive path has no parent directory")?;
    let canonical_parent = canonical_directory(parent).map_err(|error| error.to_string())?;
    let parent = local_drive_path(&canonical_parent).map_err(|error| error.to_string())?;
    let parent_handle = open_local_directory(&parent).map_err(|error| {
        format!(
            "could not open extraction parent '{}': {error}",
            parent.display()
        )
    })?;
    verify_single_link(&parent_handle, None).map_err(|error| error.to_string())?;
    let mut parent_handles = vec![parent_handle];
    let archive_path = canonical_parent.join(
        clean_archive
            .file_name()
            .ok_or("archive path has no filename")?,
    );
    let file = open_local_file(&archive_path, settings.delete_archive_after_extract)
        .map_err(|error| error.to_string())?;
    verify_handle_path(&file, &clean_archive).map_err(|error| error.to_string())?;
    let source = verify_single_link(&file, None).map_err(|error| error.to_string())?;
    after_open();
    let mut target = ExtractionTarget {
        canonical_root: parent,
        source,
    };
    if settings.create_subfolder {
        let (root, handles) = target
            .ensure_safe_subdir(&[archive_subfolder_name(&clean_archive)])
            .map_err(|error| error.to_string())?;
        target.canonical_root = root;
        parent_handles.extend(handles);
    }

    let mut state = ExtractState::default();
    let overwrite = settings.overwrite_existing;
    match format {
        ArchiveFormat::Zip => extract_zip(&file, &target, overwrite, &mut state)?,
        ArchiveFormat::Tar => extract_tar(BufReader::new(&file), &target, overwrite, &mut state)?,
        ArchiveFormat::TarGz => {
            extract_tar(MultiGzDecoder::new(&file), &target, overwrite, &mut state)?
        }
        ArchiveFormat::SevenZ => {
            let length = file.metadata().map_err(|error| error.to_string())?.len();
            extract_7z(&file, length, &target, overwrite, &mut state)?;
        }
    }

    let deleted_archive = settings.delete_archive_after_extract
        && state.files_extracted > 0
        && verify_handle_path(&file, &archive_path).is_ok()
        && file_identity(&file).is_ok_and(|identity| identity == (source, 1))
        && delete_open_file(&file).is_ok();
    drop(file);

    Ok(ExtractionSummary {
        destination_dir: target.canonical_root,
        files_extracted: state.files_extracted,
        skipped_existing: state.skipped_existing,
        total_bytes: state.total_bytes,
        deleted_archive,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "slipstream-extractor-test-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        for (name, bytes) in entries {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn detects_supported_archives_case_insensitively() {
        assert!(is_supported_archive(Path::new("C:\\DL\\pack.ZIP")));
        assert!(is_supported_archive(Path::new("C:\\DL\\src.tar.gz")));
        assert!(is_supported_archive(Path::new("C:\\DL\\src.TGZ")));
        assert!(is_supported_archive(Path::new("C:\\DL\\bundle.tar")));
        assert!(is_supported_archive(Path::new("C:\\DL\\docs.7z")));
        assert!(!is_supported_archive(Path::new("C:\\DL\\video.mp4")));
    }

    #[test]
    fn extracts_zip_creates_subfolder_writes_motw_and_deletes_source_when_requested() {
        let dir = TempDir::new();
        let archive = dir.0.join("bundle.zip");
        write_zip(&archive, &[("nested/hello.txt", b"hello world")]);

        let settings = AutoExtractSettings {
            enabled: true,
            create_subfolder: true,
            delete_archive_after_extract: true,
            overwrite_existing: false,
        };
        let summary = extract_archive(&archive, &settings).unwrap();
        assert_eq!(summary.files_extracted, 1);
        assert!(summary.deleted_archive);
        assert!(!archive.exists());
        let extracted = dir.0.join("bundle").join("nested").join("hello.txt");
        assert_eq!(fs::read(&extracted).unwrap(), b"hello world");

        #[cfg(windows)]
        {
            let mut ads = extracted.into_os_string();
            ads.push(":Zone.Identifier");
            assert_eq!(fs::read(ads).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n");
        }
    }

    #[test]
    fn extracts_tar_gz_and_respects_overwrite_false() {
        let dir = TempDir::new();
        let archive = dir.0.join("release.tar.gz");
        let existing_dir = dir.0.join("release");
        fs::create_dir_all(&existing_dir).unwrap();
        fs::write(existing_dir.join("keep.txt"), b"original").unwrap();

        let gz = GzEncoder::new(File::create(&archive).unwrap(), Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (name, body) in [
            ("keep.txt", &b"replacement"[..]),
            ("new.txt", &b"fresh"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();

        let summary = extract_archive(
            &archive,
            &AutoExtractSettings {
                enabled: true,
                create_subfolder: true,
                delete_archive_after_extract: false,
                overwrite_existing: false,
            },
        )
        .unwrap();
        assert_eq!(summary.files_extracted, 1);
        assert_eq!(summary.skipped_existing, 1);
        assert!(!summary.deleted_archive);
        assert_eq!(
            fs::read(existing_dir.join("keep.txt")).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(existing_dir.join("new.txt")).unwrap(), b"fresh");
    }

    #[test]
    fn rejects_zip_slip_reserved_names_and_symlinks_without_deleting_archive() {
        let dir = TempDir::new();
        let bad_zip = dir.0.join("evil.zip");
        write_zip(&bad_zip, &[("../outside.txt", b"pwned")]);
        let settings = AutoExtractSettings {
            enabled: true,
            create_subfolder: false,
            delete_archive_after_extract: true,
            overwrite_existing: true,
        };
        assert!(extract_archive(&bad_zip, &settings).is_err());
        assert!(bad_zip.exists());

        let reserved_zip = dir.0.join("reserved.zip");
        write_zip(&reserved_zip, &[("CON.txt", b"reserved")]);
        assert!(extract_archive(&reserved_zip, &settings).is_err());

        let symlink_tar = dir.0.join("symlink.tar");
        {
            let mut builder = tar::Builder::new(File::create(&symlink_tar).unwrap());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            builder
                .append_link(&mut header, "link.txt", "C:\\Windows\\System32\\cmd.exe")
                .unwrap();
            builder.finish().unwrap();
        }
        assert!(extract_archive(&symlink_tar, &settings).is_err());
    }

    #[test]
    fn rejects_zip_with_oversized_declared_entry_before_extraction() {
        let dir = TempDir::new();
        let fake = ExtractState::default();
        assert!(fake
            .check_declared_size(MAX_TOTAL_EXTRACTED_BYTES + 1)
            .is_err());
        assert!(validate_entry_components("/etc/passwd", false).is_err());
        assert!(validate_entry_components("C:\\evil.exe", false).is_err());
        let _ = dir;
    }

    #[test]
    fn drains_skipped_file_before_reading_next_solid_entry() {
        let dir = TempDir::new();
        fs::write(dir.0.join("existing.txt"), b"keep").unwrap();
        let archive = dir.0.join("source.7z");
        fs::write(&archive, b"fixture").unwrap();
        let file = open_local_file(&archive, false).unwrap();
        let target = ExtractionTarget {
            canonical_root: local_drive_path(&canonical_directory(&dir.0).unwrap()).unwrap(),
            source: file_identity(&file).unwrap().0,
        };
        let mut state = ExtractState::default();
        let mut solid = io::Cursor::new(b"skipread");
        write_file_entry(
            &target,
            &["existing.txt".into()],
            &mut (&mut solid).take(4),
            4,
            false,
            &mut state,
        )
        .unwrap();
        write_file_entry(
            &target,
            &["next.txt".into()],
            &mut (&mut solid).take(4),
            4,
            false,
            &mut state,
        )
        .unwrap();
        assert_eq!(fs::read(dir.0.join("existing.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(dir.0.join("next.txt")).unwrap(), b"read");
        assert_eq!(state.skipped_existing, 1);
        assert_eq!(state.bytes_consumed, 8);
        assert_eq!(state.total_bytes, 4);
    }

    #[test]
    fn stages_the_verified_mark_before_publishing_the_filename() {
        let dir = TempDir::new();
        let temporary = dir.0.join(".slipstream-tmp-test");
        let output = dir.0.join("published.txt");
        let mut stage = StagedFile::create(&temporary).unwrap();
        stage.file.write_all(b"contents").unwrap();
        stage.file.sync_all().unwrap();
        let _zone = stage_zone_identifier(&temporary, &stage.file).unwrap();
        let mut staged_zone = temporary.as_os_str().to_owned();
        staged_zone.push(":Zone.Identifier");
        assert_eq!(fs::read(staged_zone).unwrap(), ZONE_IDENTIFIER);
        assert!(!output.exists());
        publish(&stage.file, &output, false).unwrap();
        stage.published = true;
        let mut published_zone = output.into_os_string();
        published_zone.push(":Zone.Identifier");
        assert_eq!(fs::read(published_zone).unwrap(), ZONE_IDENTIFIER);
        assert!(!temporary.exists());
    }

    #[test]
    fn atomically_replaces_a_regular_file_with_a_marked_file() {
        let dir = TempDir::new();
        let archive = dir.0.join("bundle.zip");
        let output = dir.0.join("file.txt");
        fs::write(&output, b"old").unwrap();
        write_zip(&archive, &[("file.txt", b"new")]);
        let settings = AutoExtractSettings {
            enabled: true,
            create_subfolder: false,
            delete_archive_after_extract: false,
            overwrite_existing: true,
        };
        extract_archive(&archive, &settings).unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"new");
        let mut zone = output.into_os_string();
        zone.push(":Zone.Identifier");
        assert_eq!(fs::read(zone).unwrap(), ZONE_IDENTIFIER);
        assert!(fs::read_dir(&dir.0).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".slipstream-tmp-")));
    }

    #[test]
    fn rejects_hardlinked_source_and_target_and_the_source_itself() {
        let dir = TempDir::new();
        let archive = dir.0.join("bundle.zip");
        let settings = AutoExtractSettings {
            enabled: true,
            create_subfolder: false,
            delete_archive_after_extract: true,
            overwrite_existing: true,
        };
        write_zip(&archive, &[("file.txt", b"replacement")]);
        let archive_alias = dir.0.join("alias.zip");
        fs::hard_link(&archive, &archive_alias).unwrap();
        assert!(extract_archive(&archive, &settings)
            .unwrap_err()
            .contains("multiple hard links"));
        assert!(archive.is_file() && archive_alias.is_file());
        fs::remove_file(archive_alias).unwrap();

        let original = dir.0.join("original.txt");
        let existing = dir.0.join("file.txt");
        fs::write(&original, b"original").unwrap();
        fs::hard_link(&original, &existing).unwrap();
        assert!(extract_archive(&archive, &settings)
            .unwrap_err()
            .contains("multiple hard links"));
        assert_eq!(fs::read(&original).unwrap(), b"original");
        assert_eq!(fs::read(existing).unwrap(), b"original");

        let alias_entry = dir.0.join("self.zip");
        write_zip(&alias_entry, &[("self.zip", b"replacement")]);
        let before = fs::read(&alias_entry).unwrap();
        assert!(extract_archive(&alias_entry, &settings)
            .unwrap_err()
            .contains("source alias"));
        assert_eq!(fs::read(alias_entry).unwrap(), before);
    }

    #[test]
    fn rejects_corrupt_gzip_trailer_and_keeps_source() {
        let dir = TempDir::new();
        let archive = dir.0.join("bundle.tar.gz");
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "file.txt", &b"data"[..])
            .unwrap();
        let mut bytes = tar.into_inner().unwrap().finish().unwrap();
        let crc_offset = bytes.len() - 8;
        bytes[crc_offset] ^= 1;
        fs::write(&archive, bytes).unwrap();
        let settings = AutoExtractSettings {
            enabled: true,
            create_subfolder: false,
            delete_archive_after_extract: true,
            overwrite_existing: false,
        };
        assert!(extract_archive(&archive, &settings)
            .unwrap_err()
            .contains("matching checksum"));
        assert!(archive.is_file());
    }

    #[test]
    fn rejects_junction_destination_before_extracting() {
        let dir = TempDir::new();
        let outside = dir.0.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let junction = dir.0.join("evil");
        let output = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction.to_string_lossy(),
                &outside.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());

        let archive = dir.0.join("evil.zip");
        write_zip(&archive, &[("payload.txt", b"should not land outside")]);
        let err = extract_archive(
            &archive,
            &AutoExtractSettings {
                enabled: true,
                create_subfolder: true,
                delete_archive_after_extract: false,
                overwrite_existing: true,
            },
        )
        .unwrap_err();
        assert!(!err.is_empty());
        assert!(!outside.join("payload.txt").exists());
    }
}
