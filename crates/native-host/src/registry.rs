use crate::{
    manifest::{generate, Browser, HostManifest},
    security::wide,
};
use slipstream_protocol::NATIVE_HOST_NAME;
use std::{
    fs, io,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR},
    System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    },
};

pub const DEFAULT_BASE_KEY: &str = "Software";

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

pub fn host_key(base_key: &str, browser: Browser) -> String {
    format!(
        r"{base_key}\{}\NativeMessagingHosts\{NATIVE_HOST_NAME}",
        browser.registry_subkey()
    )
}

fn register(base_key: &str, browser: Browser, manifest: &Path) -> io::Result<()> {
    let key = wide(host_key(base_key, browser))?;
    let path = wide(manifest)?;
    let mut handle = null_mut();
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null(),
            &mut handle,
            null_mut(),
        )
    })?;
    let key = Key(handle);
    check(unsafe {
        RegSetValueExW(
            key.0,
            null(),
            0,
            REG_SZ,
            path.as_ptr().cast(),
            (path.len() * 2) as u32,
        )
    })
}

pub fn install_hosts(
    base_key: &str,
    directory: &Path,
    executable: &Path,
    origins: &[String],
    firefox_ids: &[String],
) -> io::Result<Vec<PathBuf>> {
    let directory = std::path::absolute(directory)?;
    // Validate all manifests before touching disk or the registry.
    let manifests = Browser::ALL.map(|browser| generate(browser, executable, origins, firefox_ids));
    let manifests = manifests.into_iter().collect::<io::Result<Vec<_>>>()?;
    let mut paths = Vec::new();
    for (browser, manifest) in Browser::ALL.into_iter().zip(manifests) {
        paths.push(write_manifest(&directory, browser, &manifest)?);
    }
    for (browser, path) in Browser::ALL.into_iter().zip(&paths) {
        register(base_key, browser, path)?;
    }
    Ok(paths)
}

pub fn install_host(
    base_key: &str,
    directory: &Path,
    browser: Browser,
    executable: &Path,
    allowlist: &[String],
) -> io::Result<PathBuf> {
    let manifest = generate(browser, executable, allowlist, allowlist)?;
    let path = write_manifest(&std::path::absolute(directory)?, browser, &manifest)?;
    register(base_key, browser, &path)?;
    Ok(path)
}

fn write_manifest(
    directory: &Path,
    browser: Browser,
    manifest: &HostManifest<'_>,
) -> io::Result<PathBuf> {
    let parent = directory.join(browser.directory());
    let path = parent.join(format!("{NATIVE_HOST_NAME}.json"));
    fs::create_dir_all(&parent)?;
    let temporary = parent.join(format!(".manifest-{}.tmp", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(manifest)?;
    if let Err(error) = fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, &path)) {
        let _ = fs::remove_file(temporary);
        return Err(error);
    }
    Ok(path)
}

pub fn uninstall_hosts(base_key: &str) -> io::Result<()> {
    let mut result = Ok(());
    for browser in Browser::ALL {
        let key = wide(host_key(base_key, browser))?;
        match unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, key.as_ptr()) } {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {}
            status if result.is_ok() => result = check(status),
            _ => {}
        }
    }
    result
}
