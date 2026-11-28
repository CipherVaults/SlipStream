use crate::{
    paths::{
        ensure_safe_directory, open_local_directory, open_unlinked_local_file,
        reject_existing_links,
    },
    settings::{read_json_from, write_json_atomically},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::{collections::BTreeMap, io, path::Path, ptr, slice};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    },
};

const VAULT_FILE: &str = "vault.dat";
const ENTROPY_DOMAIN: &[u8] = b"SlipStream.Vault.v1";

#[derive(Clone, Default)]
pub struct SecretVault {
    secrets: BTreeMap<String, String>,
}

impl SecretVault {
    pub fn load(data_dir: &Path) -> io::Result<Self> {
        let path = reject_existing_links(&data_dir.join(VAULT_FILE))?;
        let secrets = read_json_from(open_unlinked_local_file(&path))?;
        Ok(Self { secrets })
    }

    pub fn set_secret(&mut self, data_dir: &Path, key: &str, value: &str) -> io::Result<()> {
        let encrypted = dpapi(key, value.as_bytes(), false).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("Windows DPAPI could not protect the vault secret: {error}"),
            )
        })?;
        let mut updated = self.secrets.clone();
        updated.insert(key.into(), STANDARD.encode(encrypted.bytes()));
        self.save(data_dir, updated)
    }

    pub fn get_secret(&self, key: &str) -> io::Result<Option<String>> {
        self.secrets
            .get(key)
            .map(|encoded| {
                let encrypted = STANDARD.decode(encoded).map_err(|_| invalid_ciphertext())?;
                let plain = dpapi(key, &encrypted, true)?;
                std::str::from_utf8(plain.bytes())
                    .map(str::to_owned)
                    .map_err(|_| invalid_ciphertext())
            })
            .transpose()
    }

    pub fn delete_secret(&mut self, data_dir: &Path, key: &str) -> io::Result<()> {
        if !self.has_secret(key) {
            return Ok(());
        }
        let mut updated = self.secrets.clone();
        updated.remove(key);
        self.save(data_dir, updated)
    }

    pub fn has_secret(&self, key: &str) -> bool {
        self.secrets.contains_key(key)
    }

    pub fn clear(&mut self, data_dir: &Path) -> io::Result<()> {
        self.save(data_dir, BTreeMap::new())
    }

    pub(crate) fn restore(&mut self, data_dir: &Path, previous: Self) -> io::Result<()> {
        self.save(data_dir, previous.secrets)
    }

    fn save(&mut self, data_dir: &Path, updated: BTreeMap<String, String>) -> io::Result<()> {
        let directory = ensure_safe_directory(data_dir)?;
        let _directory = open_local_directory(&directory)?;
        let path = reject_existing_links(&directory.join(VAULT_FILE))?;
        match open_unlinked_local_file(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        write_json_atomically(&path, &updated)?;
        self.secrets = updated;
        Ok(())
    }
}

pub fn proxy_secret_key() -> &'static str {
    "proxy.password"
}

pub fn site_login_secret_key(id: &str) -> String {
    format!("site-login.{id}")
}

fn blob(data: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "vault input too large"))?,
        pbData: data.as_ptr().cast_mut(),
    })
}

struct LocalBlob {
    blob: CRYPT_INTEGER_BLOB,
    plaintext: bool,
}

impl LocalBlob {
    fn bytes(&self) -> &[u8] {
        if self.blob.cbData == 0 {
            return &[];
        }
        // DPAPI supplies exactly cbData initialized bytes in its local allocation.
        unsafe { slice::from_raw_parts(self.blob.pbData, self.blob.cbData as usize) }
    }
}

impl Drop for LocalBlob {
    fn drop(&mut self) {
        if !self.blob.pbData.is_null() {
            unsafe {
                if self.plaintext {
                    for index in 0..self.blob.cbData as usize {
                        self.blob.pbData.add(index).write_volatile(0);
                    }
                }
                LocalFree(self.blob.pbData.cast());
            }
        }
    }
}

fn invalid_ciphertext() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid vault ciphertext")
}

fn dpapi(key: &str, data: &[u8], decrypt: bool) -> io::Result<LocalBlob> {
    let input = blob(data)?;
    // Binding the key into the entropy prevents ciphertext swaps between slots.
    let entropy = [ENTROPY_DOMAIN, key.as_bytes()].concat();
    let entropy = blob(&entropy)?;
    let mut output = LocalBlob {
        blob: CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        },
        plaintext: decrypt,
    };
    let success = unsafe {
        if decrypt {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.blob,
            )
        } else {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.blob,
            )
        }
    };
    if success != 0 {
        Ok(output)
    } else if decrypt {
        Err(invalid_ciphertext())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::OnceLock};
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;

    fn current_user_dpapi_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| match dpapi("test", b"test", false) {
            Ok(_) => true,
            // Windows returns this when the test runner has no loaded DPAPI user profile.
            Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                eprintln!("CurrentUser DPAPI unavailable in this Windows test runner: {error}");
                false
            }
            Err(error) => panic!("CurrentUser DPAPI probe failed: {error}"),
        })
    }

    #[test]
    fn round_trip_across_reload() {
        if !current_user_dpapi_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut vault = SecretVault::load(dir.path()).unwrap();
        let key = proxy_secret_key();
        vault
            .set_secret(dir.path(), key, "secret for testing")
            .unwrap();
        let reloaded = SecretVault::load(dir.path()).unwrap();
        assert_eq!(
            reloaded.get_secret(key).unwrap().as_deref(),
            Some("secret for testing")
        );
        assert!(!fs::read_to_string(dir.path().join(VAULT_FILE))
            .unwrap()
            .contains("secret for testing"));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = SecretVault::load(dir.path()).unwrap();
        let key = proxy_secret_key();
        let mut ciphertext = if current_user_dpapi_available() {
            vault.set_secret(dir.path(), key, "secret").unwrap();
            STANDARD.decode(&vault.secrets[key]).unwrap()
        } else {
            // Still check that invalid encrypted data loaded from disk fails closed.
            b"invalid ciphertext".to_vec()
        };
        let middle = ciphertext.len() / 2;
        ciphertext[middle] ^= 1;
        vault
            .secrets
            .insert(key.into(), STANDARD.encode(ciphertext));
        let updated = vault.secrets.clone();
        vault.save(dir.path(), updated).unwrap();
        let reloaded = SecretVault::load(dir.path()).unwrap();
        assert_eq!(
            reloaded.get_secret(key).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn deletion_clears_the_key_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = SecretVault::load(dir.path()).unwrap();
        let key = site_login_secret_key("one");
        let encrypted = BTreeMap::from([(key.clone(), STANDARD.encode(b"fixture ciphertext"))]);
        vault.save(dir.path(), encrypted).unwrap();
        assert!(SecretVault::load(dir.path()).unwrap().has_secret(&key));
        vault.delete_secret(dir.path(), &key).unwrap();
        assert!(!vault.has_secret(&key));
        assert_eq!(
            SecretVault::load(dir.path())
                .unwrap()
                .get_secret(&key)
                .unwrap(),
            None
        );
    }
}
