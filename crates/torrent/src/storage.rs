use crate::{parse_torrent_bytes, PieceBitmap, TorrentError};
use librqbit::{
    storage::{
        filesystem::FilesystemStorageFactory, BoxStorageFactory, StorageFactory, StorageFactoryExt,
        TorrentStorage,
    },
    AddTorrentOptions, ManagedTorrentShared, TorrentMetadata,
};
use librqbit_core::lengths::ValidPieceIndex;
use slipstream_protocol::{safe_join_under, PathSecurityError};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
};

pub(crate) type SharedBitmap = Arc<RwLock<PieceBitmap>>;

/// Pairs librqbit's output folder with the path checked by the storage wrapper.
#[derive(Clone)]
pub struct ConfinedStorageFactory {
    base: PathBuf,
    output: PathBuf,
    bitmap: SharedBitmap,
    from_v1_wire: bool,
}

impl ConfinedStorageFactory {
    pub(crate) fn new(
        base: &Path,
        output: &Path,
        bitmap: SharedBitmap,
        from_v1_wire: bool,
    ) -> Self {
        Self {
            base: base.to_owned(),
            output: output.to_owned(),
            bitmap,
            from_v1_wire,
        }
    }

    pub(crate) fn with_output(
        self,
        mut options: AddTorrentOptions,
    ) -> Result<AddTorrentOptions, TorrentError> {
        validate_output(&self.base, &self.output)?;
        // ManagedTorrentShared::options is private in librqbit 8.1.1. Supply the
        // same explicit folder to both sides so automatic subfolders are bypassed.
        options.output_folder = Some(path_text(&self.output)?.to_owned());
        options.sub_folder = None;
        options.storage_factory = Some(self.boxed());
        Ok(options)
    }
}

impl StorageFactory for ConfinedStorageFactory {
    type Storage = ConfinedTorrentStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(ConfinedTorrentStorage {
            inner: Box::new(FilesystemStorageFactory::default().create(shared, metadata)?),
            factory: self.clone(),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

pub struct ConfinedTorrentStorage {
    inner: Box<dyn TorrentStorage>,
    factory: ConfinedStorageFactory,
}

impl TorrentStorage for ConfinedTorrentStorage {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        let output = &self.factory.output;
        validate_output(&self.factory.base, output)?;
        let manifest = if self.factory.from_v1_wire {
            crate::metadata::parse_wire_metadata(&metadata.torrent_bytes, output)?
        } else {
            parse_torrent_bytes(&metadata.torrent_bytes, output)?
        };
        if manifest.file_specs.len() != metadata.file_infos.len() {
            return Err(
                TorrentError::Metadata("storage file count differs from manifest".into()).into(),
            );
        }
        for (index, file) in metadata.file_infos.iter().enumerate() {
            if file.attrs.symlink {
                return Err(
                    TorrentError::Unsupported("symlink entries are forbidden".into()).into(),
                );
            }
            let path = safe_join_under(output, path_text(&file.relative_filename)?)?;
            if path != manifest.resolved_paths[index]
                || file.attrs.padding != manifest.file_specs[index].is_padding
                || file.len != manifest.file_specs[index].size_bytes
            {
                return Err(
                    TorrentError::Metadata("storage layout differs from manifest".into()).into(),
                );
            }
            if !file.attrs.padding {
                reject_existing_links(&path)?;
            }
        }

        self.inner.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.inner.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.inner.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        reject_existing_links(&safe_join_under(
            &self.factory.output,
            path_text(filename)?,
        )?)?;
        self.inner.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        if path.as_os_str().is_empty() || path == self.factory.output {
            if self.factory.output == self.factory.base {
                return Ok(());
            }
            validate_output(&self.factory.base, &self.factory.output)?;
            return self.inner.remove_directory_if_empty(Path::new(""));
        }
        reject_existing_links(&safe_join_under(&self.factory.output, path_text(path)?)?)?;
        self.inner.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.inner.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            inner: self.inner.take()?,
            factory: self.factory.clone(),
        }))
    }

    fn on_piece_completed(&self, piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        self.inner.on_piece_completed(piece_index)?;
        self.factory
            .bitmap
            .write()
            .unwrap()
            .mark_completed(piece_index.get_usize());
        Ok(())
    }
}

pub(crate) fn path_text(path: &Path) -> Result<&str, TorrentError> {
    path.to_str()
        .ok_or_else(|| PathSecurityError::InvalidCharacter.into())
}

pub(crate) fn validate_output(base: &Path, output: &Path) -> Result<(), TorrentError> {
    if !base.is_absolute()
        || !output.is_absolute()
        || !output.starts_with(base)
        || output
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(PathSecurityError::EscapesBaseDir.into());
    }
    let relative = output
        .strip_prefix(base)
        .map_err(|_| PathSecurityError::EscapesBaseDir)?;
    if !relative.as_os_str().is_empty() {
        safe_join_under(base, path_text(relative)?)?;
    }
    reject_existing_links(output)
}

fn reject_existing_links(path: &Path) -> Result<(), TorrentError> {
    // A symlink or junction already present on disk can also redirect a safe relative path.
    for ancestor in path.ancestors().filter(|path| path.is_absolute()) {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(PathSecurityError::EscapesBaseDir.into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
