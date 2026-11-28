use super::path_text;
use anyhow::{ensure, Context};
use librqbit::{storage::TorrentStorage, ManagedTorrentShared, TorrentMetadata};
use slipstream_protocol::safe_join_under;
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::windows::{
        fs::{FileExt, MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
    sync::RwLock,
};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        ReOpenFile, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

const SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

pub(super) fn is_archive(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            [".zip", ".tar", ".tar.gz", ".tgz", ".7z"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
        })
}

struct Payload {
    file: File,
    archive: bool,
}

impl Payload {
    fn with_writer<T>(&self, write: impl FnOnce(&File) -> io::Result<T>) -> io::Result<T> {
        if self.archive {
            // Seed with a read-only handle. The extractor can then deny
            // writes for its entire read without disconnecting the seeder.
            write(&reopen(&self.file, GENERIC_WRITE)?)
        } else {
            write(&self.file)
        }
    }
}

fn reopen(file: &File, access: u32) -> io::Result<File> {
    // ReOpenFile addresses the same file object, even if its original name
    // was replaced after the torrent opened it.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            access,
            SHARE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(super) struct ArchiveStorage {
    root: PathBuf,
    overwrite: bool,
    payloads: RwLock<Vec<Option<Payload>>>,
}

impl ArchiveStorage {
    pub(super) fn new(root: PathBuf, overwrite: bool) -> Self {
        Self {
            root,
            overwrite,
            payloads: RwLock::default(),
        }
    }

    fn with_payload<T>(
        &self,
        index: usize,
        operation: impl FnOnce(&Payload) -> io::Result<T>,
    ) -> anyhow::Result<T> {
        let payloads = self.payloads.read().unwrap();
        let payload = payloads
            .get(index)
            .and_then(Option::as_ref)
            .context("no torrent payload file")?;
        Ok(operation(payload)?)
    }
}

impl TorrentStorage for ArchiveStorage {
    fn init(&mut self, _: &ManagedTorrentShared, metadata: &TorrentMetadata) -> anyhow::Result<()> {
        let mut payloads = Vec::with_capacity(metadata.file_infos.len());
        for file in &metadata.file_infos {
            if file.attrs.padding {
                payloads.push(None);
                continue;
            }
            let path = safe_join_under(&self.root, path_text(&file.relative_filename)?)?;
            fs::create_dir_all(path.parent().context("torrent payload has no parent")?)?;
            let handle = OpenOptions::new()
                .read(true)
                .write(true)
                .create(self.overwrite)
                .create_new(!self.overwrite)
                .truncate(false)
                .share_mode(SHARE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)
                .with_context(|| format!("could not open torrent payload {}", path.display()))?;
            let attributes = handle.metadata()?;
            ensure!(
                attributes.is_file()
                    && attributes.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                "torrent payload is a reparse point or non-file"
            );
            let archive = is_archive(&path);
            let file = if archive {
                reopen(&handle, GENERIC_READ)?
            } else {
                handle
            };
            payloads.push(Some(Payload { file, archive }));
        }
        *self.payloads.get_mut().unwrap() = payloads;
        Ok(())
    }

    fn pread_exact(&self, index: usize, mut offset: u64, mut buf: &mut [u8]) -> anyhow::Result<()> {
        self.with_payload(index, move |payload| {
            while !buf.is_empty() {
                let len = payload.file.seek_read(buf, offset)?;
                if len == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                buf = &mut buf[len..];
                offset += len as u64;
            }
            Ok(())
        })
    }

    fn pwrite_all(&self, index: usize, mut offset: u64, mut buf: &[u8]) -> anyhow::Result<()> {
        self.with_payload(index, move |payload| {
            payload.with_writer(|file| {
                while !buf.is_empty() {
                    let len = file.seek_write(buf, offset)?;
                    if len == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    buf = &buf[len..];
                    offset += len as u64;
                }
                Ok(())
            })
        })
    }

    fn remove_file(&self, _: usize, filename: &Path) -> anyhow::Result<()> {
        Ok(fs::remove_file(self.root.join(filename))?)
    }

    fn remove_directory_if_empty(&self, relative: &Path) -> anyhow::Result<()> {
        match fs::remove_dir(self.root.join(relative)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn ensure_file_length(&self, index: usize, len: u64) -> anyhow::Result<()> {
        self.with_payload(index, |payload| {
            if payload.file.metadata()?.len() == len {
                return Ok(());
            }
            payload.with_writer(|file| file.set_len(len))
        })
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        let payloads = std::mem::take(&mut *self.payloads.write().unwrap());
        Ok(Box::new(Self {
            root: self.root.clone(),
            overwrite: self.overwrite,
            payloads: RwLock::new(payloads),
        }))
    }
}
