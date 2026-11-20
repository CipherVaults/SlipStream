use crate::security::{current_user_sid, wide, SecurityDescriptor};
use rand::{rngs::OsRng, RngCore};
use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    os::windows::io::FromRawHandle,
    path::{Path, PathBuf},
    ptr::null_mut,
};
use subtle::ConstantTimeEq;
use windows_sys::Win32::{
    Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL},
};

const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_BYTES: usize = TOKEN_BYTES * 2;

pub fn local_app_directory() -> io::Result<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(|path| PathBuf::from(path).join("SlipStream"))
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable"))
}

pub fn default_token_path() -> io::Result<PathBuf> {
    Ok(local_app_directory()?.join("session.token"))
}

pub struct SessionToken(String);

impl SessionToken {
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0; TOKEN_BYTES];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| io::Error::other("secure random generation failed"))?;
        Ok(Self(hex::encode(bytes)))
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        let mut data = Vec::new();
        File::open(path)?
            .take(TOKEN_HEX_BYTES as u64 + 1)
            .read_to_end(&mut data)?;
        if data.len() != TOKEN_HEX_BYTES
            || !data.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid session token file",
            ));
        }
        Ok(Self(String::from_utf8(data).expect("validated ASCII hex")))
    }

    pub(crate) fn publish(&self, path: &Path) -> io::Result<()> {
        let path = std::path::absolute(path)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "token path needs a parent")
        })?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".session-{}.tmp", uuid::Uuid::new_v4()));
        // Protect the token even if the containing directory inherits permissive ACLs.
        let descriptor = SecurityDescriptor::new(&format!("D:P(A;;GA;;;{})", current_user_sid()?))?;
        let handle = unsafe {
            CreateFileW(
                wide(&temporary)?.as_ptr(),
                GENERIC_WRITE,
                0,
                &descriptor.attributes(),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_handle(handle) };
        let result = file
            .write_all(self.0.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        let result = result.and_then(|()| fs::rename(&temporary, path));
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub fn verify(&self, candidate: &str) -> bool {
        bool::from(self.0.as_bytes().ct_eq(candidate.as_bytes()))
    }
}
