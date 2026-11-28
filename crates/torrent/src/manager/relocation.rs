use super::cleanup_empty_dirs;
use crate::{
    storage::{is_link, open_regular_file, validate_output},
    TorrentError, TorrentMetadata,
};
use slipstream_protocol::PathSecurityError;
use std::{
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Read},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(super) fn move_files(
    old_dir: PathBuf,
    target_dir: PathBuf,
    metadata: &TorrentMetadata,
    target_paths: &[PathBuf],
) -> Result<Relocation, TorrentError> {
    let mut relocation = Relocation {
        old_dir,
        target_dir,
        moved: Vec::new(),
    };
    for ((spec, src), dst) in metadata
        .file_specs
        .iter()
        .zip(&metadata.resolved_paths)
        .zip(target_paths)
    {
        if spec.is_padding {
            continue;
        }
        match move_payload(&relocation.old_dir, src, &relocation.target_dir, dst) {
            Ok(true) => relocation.moved.push((src.clone(), dst.clone())),
            Ok(false) => {}
            Err(error) => return Err(relocation.rollback(error)),
        }
    }
    Ok(relocation)
}

pub(super) struct Relocation {
    old_dir: PathBuf,
    target_dir: PathBuf,
    moved: Vec<(PathBuf, PathBuf)>,
}

impl Relocation {
    pub(super) fn rollback(self, original: TorrentError) -> TorrentError {
        let mut errors = Vec::new();
        for (src, dst) in self.moved.into_iter().rev() {
            match move_payload(&self.target_dir, &dst, &self.old_dir, &src) {
                Ok(true) => {}
                Ok(false) => errors.push(format!("{} disappeared", dst.display())),
                Err(error) => errors.push(format!("{}: {error}", dst.display())),
            }
        }
        if errors.is_empty() {
            original
        } else {
            TorrentError::Session(format!(
                "{original}. Storage rollback failed for {}",
                errors.join(", ")
            ))
        }
    }

    pub(super) fn cleanup(self) {
        if let Err(error) = cleanup_empty_dirs(
            &self.old_dir,
            self.moved.iter().map(|(src, _)| src.as_path()),
        ) {
            tracing::debug!(%error, "could not clean up old storage directories");
        }
    }
}

fn move_payload(
    src_root: &Path,
    src: &Path,
    dst_root: &Path,
    dst: &Path,
) -> Result<bool, TorrentError> {
    validate_output(src_root, src)?;
    let parent = dst.parent().ok_or(PathSecurityError::EscapesBaseDir)?;
    validate_output(dst_root, parent)?;
    match fs::symlink_metadata(dst) {
        Ok(_) => {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!("storage destination exists: {}", dst.display()),
            )
            .into());
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    validate_output(dst_root, dst)?;
    let src_metadata = match fs::symlink_metadata(src) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if is_link(&src_metadata) || !src_metadata.is_file() {
        return Err(PathSecurityError::EscapesBaseDir.into());
    }
    fs::create_dir_all(parent)?;
    validate_output(dst_root, dst)?;
    match rename_noreplace(src, dst) {
        Ok(()) => {}
        Err(error)
            if error.kind() == ErrorKind::CrossesDevices
                || (cfg!(windows) && error.raw_os_error() == Some(17)) =>
        {
            copy_then_remove(src, dst, src_metadata.len())?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(true)
}

fn copy_then_remove(src: &Path, dst: &Path, expected_len: u64) -> Result<(), TorrentError> {
    let mut input = open_regular_file(src)?.take(expected_len.saturating_add(1));
    let temp = dst.with_file_name(format!(".slipstream-move-{}.tmp", Uuid::new_v4()));
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    let copied = (|| -> io::Result<()> {
        if io::copy(&mut input, &mut output)? != expected_len {
            return Err(io::Error::other(
                "source length changed during storage move",
            ));
        }
        output.sync_all()
    })();
    drop(output);
    drop(input);
    if let Err(error) = copied.and_then(|()| rename_noreplace(&temp, dst)) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    let result = fs::symlink_metadata(dst).and_then(|metadata| {
        if is_link(&metadata) || !metadata.is_file() || metadata.len() != expected_len {
            return Err(io::Error::other("could not verify copied payload length"));
        }
        fs::remove_file(src)
    });
    if let Err(error) = result {
        undo_publication(dst, &error)?;
        return Err(error.into());
    }
    Ok(())
}

fn undo_publication(dst: &Path, original: &io::Error) -> io::Result<()> {
    fs::remove_file(dst).map_err(|error| {
        io::Error::other(format!(
            "{original}. Could not roll back {}: {error}",
            dst.display()
        ))
    })
}

#[cfg(windows)]
fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    // With no flags, MoveFileExW fails if the destination exists or is on another volume.
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let src = wide(src);
    let dst = wide(dst);
    if unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn copy_fallback_preserves_original_until_verified_and_published() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("destination");
        fs::write(&source, b"original payload").unwrap();
        assert!(copy_then_remove(&source, &dest, 2).is_err());
        assert!(!dest.exists());
        assert_eq!(fs::read(&source).unwrap(), b"original payload");
        fs::write(&dest, b"another file").unwrap();
        assert!(copy_then_remove(&source, &dest, 16).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"another file");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        fs::remove_file(&dest).unwrap();

        copy_then_remove(&source, &dest, 16).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"original payload");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[cfg(not(windows))]
fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    // hard_link atomically claims the new name without replacing an existing destination.
    fs::hard_link(src, dst)?;
    if let Err(error) = fs::remove_file(src) {
        undo_publication(dst, &error)?;
        return Err(error);
    }
    Ok(())
}
