use base64::{engine::general_purpose::STANDARD, Engine as _};
use minisign::{SecretKey, SecretKeyBox};
use semver::Version;
use std::{
    borrow::Cow,
    env,
    error::Error,
    ffi::OsString,
    fs::{self, File},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const USAGE: &str =
    "Usage:\n  slipstream-sign --file <path> --version <semver> --out <sig_path> [--key <path_or_string>]\n  slipstream-sign --verify --file <path> --version <semver> --sig <sig_text_or_path>";
const PRIVATE_KEY: &str = "TAURI_SIGNING_PRIVATE_KEY";
const PASSWORD: &str = "TAURI_SIGNING_PRIVATE_KEY_PASSWORD";

#[derive(Default)]
struct Args {
    verify: bool,
    file: Option<OsString>,
    version: Option<OsString>,
    out: Option<OsString>,
    sig: Option<OsString>,
    key: Option<OsString>,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut parsed = Self::default();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            if flag.to_str() == Some("--verify") {
                if parsed.verify {
                    return Err(format!("Duplicate --verify argument.\n{USAGE}").into());
                }
                parsed.verify = true;
                continue;
            }
            let value = match flag.to_str() {
                Some("--file") => &mut parsed.file,
                Some("--version") => &mut parsed.version,
                Some("--out") => &mut parsed.out,
                Some("--sig") => &mut parsed.sig,
                Some("--key") => &mut parsed.key,
                _ => return Err(format!("Unrecognized argument.\n{USAGE}").into()),
            };
            if value.is_some() {
                return Err(
                    format!("Duplicate {} argument.\n{USAGE}", flag.to_string_lossy()).into(),
                );
            }
            *value = Some(
                args.next()
                    .filter(|value| !value.to_string_lossy().starts_with("--"))
                    .ok_or_else(|| {
                        format!("Missing value for {}.\n{USAGE}", flag.to_string_lossy())
                    })?,
            );
        }
        Ok(parsed)
    }

    fn run(self) -> Result<()> {
        if self.verify {
            let (Some(file), Some(version), Some(sig)) = (self.file, self.version, self.sig) else {
                return Err(USAGE.into());
            };
            let version = Version::parse(version.to_str().ok_or("Version must be valid UTF-8")?)?;
            let bytes = fs::read(&file)?;
            let sig_str = match PathBuf::from(&sig) {
                p if p.is_file() => fs::read_to_string(p)?,
                _ => sig
                    .into_string()
                    .map_err(|_| "Signature must be valid UTF-8.")?,
            };
            slipstream_desktop_lib::updater::verify_release_signature(
                slipstream_desktop_lib::updater::DEFAULT_UPDATER_PUBKEY_B64,
                &sig_str,
                &bytes,
                &version.to_string(),
            )
            .map_err(|e| -> Box<dyn Error> { e.into() })?;
            return Ok(());
        }
        let (Some(file), Some(version), Some(out)) = (self.file, self.version, self.out) else {
            return Err(USAGE.into());
        };
        let version = Version::parse(version.to_str().ok_or("Version must be valid UTF-8")?)?;
        let key = match self.key {
            Some(key) => read_key_override(key)?,
            None => {
                env::var(PRIVATE_KEY).map_err(|_| format!("Set {PRIVATE_KEY} or pass --key."))?
            }
        };
        let password = match env::var(PASSWORD) {
            Ok(password) => Some(password),
            Err(env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        };
        sign_file(Path::new(&file), &version, Path::new(&out), &key, password)
    }
}

fn read_key_override(key: OsString) -> Result<String> {
    let path = PathBuf::from(&key);
    if path.is_file() {
        return Ok(fs::read_to_string(path)?);
    }
    key.into_string()
        .map_err(|_| "The --key value must be a file path or UTF-8 key text.".into())
}

fn load_key(input: &str, password: Option<String>) -> Result<SecretKey> {
    let input = input.trim();
    let key_text = if input.starts_with("untrusted comment:") {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(String::from_utf8(STANDARD.decode(input)?)?)
    };
    let key = SecretKeyBox::from_string(key_text.trim())?;
    // minisign 0.10 uses separate loading APIs for KDF_NONE and scrypt keys.
    if let Ok(unencrypted) = SecretKey::from_unencrypted_box(key.clone()) {
        return Ok(unencrypted);
    }
    let password = password.or_else(|| (!std::io::stdin().is_terminal()).then(String::new));
    Ok(key.into_secret_key(password)?)
}

fn sign_file(
    file: &Path,
    version: &Version,
    out: &Path,
    key: &str,
    password: Option<String>,
) -> Result<()> {
    let filename = file
        .file_name()
        .ok_or("Expected a file to sign")?
        .to_string_lossy();
    if filename.contains(['\t', '\r', '\n']) {
        return Err("The filename cannot contain a tab or newline.".into());
    }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let comment = format!("timestamp:{timestamp}\tfile:{filename}\tversion:{version}");
    let reader = File::open(file)?;
    let key = load_key(key, password)?;
    let signature = minisign::sign(
        None,
        &key,
        reader,
        Some(&comment),
        Some("signature from tauri secret key"),
    )?;

    let encoded = STANDARD.encode(signature.to_string());
    let mut output = File::create_new(out)?;
    if let Err(error) = writeln!(output, "{encoded}").and_then(|()| output.sync_all()) {
        drop(output);
        let _ = fs::remove_file(out);
        return Err(error.into());
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1).peekable();
    if args
        .peek()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match Args::parse(args).and_then(Args::run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("slipstream-sign: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;
    use slipstream_desktop_lib::updater::verify_release_signature;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                env::temp_dir().join(format!("slipstream-sign-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn assert_signature_formats(keys: KeyPair, password: Option<String>) {
        let dir = TempDir::new();
        let installer = dir.0.join("SlipStream setup.exe");
        let key_file = dir.0.join("minisign.key");
        let bytes = b"test installer bytes";
        fs::write(&installer, bytes).unwrap();
        let version = Version::parse("2.0.0").unwrap();
        let key_box = String::from_utf8(keys.sk.to_box(None).unwrap().to_bytes()).unwrap();
        fs::write(&key_file, &key_box).unwrap();
        let pubkey = STANDARD.encode(keys.pk.to_box().unwrap().to_bytes());

        for (label, key) in [
            ("raw", read_key_override(key_file.into()).unwrap()),
            ("base64", format!("  {}\n", STANDARD.encode(&key_box))),
        ] {
            let output = dir.0.join(format!("{label}.sig"));
            sign_file(&installer, &version, &output, &key, password.clone()).unwrap();

            let encoded = fs::read_to_string(&output).unwrap();
            assert!(encoded.ends_with('\n'));
            verify_release_signature(&pubkey, &encoded, bytes, "2.0.0").unwrap();
            assert!(verify_release_signature(&pubkey, &encoded, bytes, "2.0.1").is_err());

            // Match the legacy updater's strict base64 decoding of the manifest value.
            let raw = String::from_utf8(STANDARD.decode(encoded.trim_end()).unwrap()).unwrap();
            let mut lines = raw.lines();
            assert_eq!(
                lines.next(),
                Some("untrusted comment: signature from tauri secret key")
            );
            assert!(lines.next().unwrap().starts_with("RU"));
            let trusted = lines.next().unwrap();
            assert!(trusted.starts_with("trusted comment: timestamp:"));
            assert!(trusted.ends_with("\tfile:SlipStream setup.exe\tversion:2.0.0"));
            assert_eq!(lines.count(), 1);

            let signature = minisign_verify::Signature::decode(&raw).unwrap();
            let public = minisign_verify::PublicKey::decode(
                &String::from_utf8(STANDARD.decode(&pubkey).unwrap()).unwrap(),
            )
            .unwrap();
            public.verify(bytes, &signature, true).unwrap();
        }
    }

    #[test]
    fn signs_with_raw_and_wrapped_unencrypted_keys() {
        assert_signature_formats(KeyPair::generate_unencrypted_keypair().unwrap(), None);
    }

    #[test]
    fn signs_with_raw_and_wrapped_password_protected_keys() {
        let password = "test password".to_owned();
        let keys = KeyPair::generate_encrypted_keypair(Some(password.clone())).unwrap();
        assert_signature_formats(keys, Some(password));
    }

    #[test]
    fn parses_release_cli_flags() {
        let args = Args::parse(
            [
                "--file",
                "setup.exe",
                "--version",
                "2.0.0",
                "--out",
                "setup.sig",
                "--key",
                "key",
            ]
            .map(OsString::from),
        )
        .unwrap();
        assert_eq!(args.file.unwrap(), "setup.exe");
        assert_eq!(args.version.unwrap(), "2.0.0");
        assert_eq!(args.out.unwrap(), "setup.sig");
        assert_eq!(args.key.unwrap(), "key");

        assert!(Args::parse(["--key", "--out", "setup.sig"].map(OsString::from)).is_err());
        assert!(Args::parse(["--out", "a", "--out", "b"].map(OsString::from)).is_err());
    }
}
