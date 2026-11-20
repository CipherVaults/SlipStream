use sha2::{Digest, Sha256};
use std::{
    ffi::{c_void, OsStr},
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    },
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER, HANDLE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub(crate) fn wide(value: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut encoded: Vec<_> = value.as_ref().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "embedded NUL"));
    }
    encoded.push(0);
    Ok(encoded)
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

pub fn current_user_sid() -> io::Result<String> {
    // GetCurrentProcess returns a pseudo-handle that must not be closed.
    unsafe { process_user_sid(GetCurrentProcess()) }
}

/// The caller must supply a valid process handle with query access.
pub(crate) unsafe fn process_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut length = 0;
    if unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length) } == 0
        && io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
    {
        return Err(io::Error::last_os_error());
    }
    // TOKEN_USER contains a pointer and requires native pointer alignment.
    let mut data = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            data.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid = LocalAllocation(sid.cast());
    let length = (0..).take_while(|&i| unsafe { *sid.add(i) } != 0).count();
    String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn pipe_name_for_sid(sid: &str) -> String {
    format!(
        r"\\.\pipe\SlipStream.IPC.{}",
        hex::encode(&Sha256::digest(sid)[..8])
    )
}

pub fn default_pipe_name() -> io::Result<String> {
    Ok(pipe_name_for_sid(&current_user_sid()?))
}

pub fn pipe_sddl(sid: &str) -> String {
    format!("D:(A;;GA;;;{sid})")
}

pub(crate) struct SecurityDescriptor(LocalAllocation);

impl SecurityDescriptor {
    pub(crate) fn new(sddl: &str) -> io::Result<Self> {
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(sddl)?.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(LocalAllocation(descriptor)))
    }

    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: 0,
        }
    }
}

pub fn validate_local_pipe(name: &str) -> io::Result<()> {
    if name.strip_prefix(r"\\.\pipe\").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.len() <= 240
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    }) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid local pipe name",
        ))
    }
}
