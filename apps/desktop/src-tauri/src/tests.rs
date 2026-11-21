use crate::{
    expand_wildcard_urls, install_browser_hosts_into,
    paths::{
        build_defender_scan_args, decode_sha256, delete_files, ensure_safe_directory, hash_file,
        local_drive_path, reveal_action, write_zone_identifier, RevealAction,
    },
    settings::{normalize, torrent_rate},
    tools::{explorer_executable, hardened_toolchain_config_with_path, native_hosts_installed},
    ToolchainStatus,
};
use serde_json::{json, Value};
use slipstream_native_host::manifest::DEFAULT_FIREFOX_ID;
use slipstream_protocol::AppSettings;
use std::{
    env,
    ffi::OsString,
    fs,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    ptr::null_mut,
};
use uuid::Uuid;
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    },
    System::{
        Ioctl::FSCTL_SET_REPARSE_POINT,
        Registry::{RegDeleteTreeW, HKEY_CURRENT_USER},
        SystemServices::IO_REPARSE_TAG_MOUNT_POINT,
        IO::DeviceIoControl,
    },
};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = env::temp_dir().join(format!("slipstream-desktop-unit-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn directory_junction(link: &Path, destination: &Path) {
    // A mount-point reparse buffer works without Windows Developer Mode or
    // symlink privileges. Offsets and lengths are bytes within PathBuffer.
    fs::create_dir(link).unwrap();
    let destination = local_drive_path(&fs::canonicalize(destination).unwrap()).unwrap();
    let substitute: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(destination.as_os_str().encode_wide())
        .collect();
    let print: Vec<u16> = destination.as_os_str().encode_wide().collect();
    let substitute_len = (substitute.len() * 2) as u16;
    let print_len = (print.len() * 2) as u16;
    let mut buffer = Vec::new();
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for value in [
        8 + substitute_len + print_len + 4,
        0,
        0,
        substitute_len,
        substitute_len + 2,
        print_len,
    ] {
        buffer.extend(value.to_le_bytes());
    }
    for unit in substitute.into_iter().chain([0]).chain(print).chain([0]) {
        buffer.extend(unit.to_le_bytes());
    }
    let handle = fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(link)
        .unwrap();
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    assert_ne!(result, 0, "{}", std::io::Error::last_os_error());
    assert_ne!(
        fs::symlink_metadata(link).unwrap().file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT,
        0
    );
}

#[test]
fn explorer_and_defender_args_preserve_discrete_paths() {
    let temp = TempDir::new();
    let root = temp.0.join("download & save");
    fs::create_dir(&root).unwrap();
    for filename in ["video, with spaces.mp4", "a,b.mp4"] {
        let file = root.join(filename);
        fs::write(&file, b"test").unwrap();
        let expected: OsString = local_drive_path(&fs::canonicalize(&file).unwrap())
            .unwrap()
            .into();
        let quoted = OsString::from(format!("/select,\"{}\"", expected.to_string_lossy()));
        assert_eq!(
            reveal_action(&root, Some(filename)).unwrap(),
            RevealAction::SelectFile(quoted)
        );
        let args = build_defender_scan_args(&root, filename).unwrap();
        assert_eq!(&args[..4], ["-Scan", "-ScanType", "3", "-File"]);
        assert_eq!(args[4], expected);
    }
    let directory = local_drive_path(&fs::canonicalize(&root).unwrap()).unwrap();
    for missing in [None, Some("missing.bin")] {
        assert_eq!(
            reveal_action(&root, missing).unwrap(),
            RevealAction::OpenDir(directory.clone())
        );
    }
    let explorer = explorer_executable().unwrap();
    assert!(explorer.is_file());
    assert!(explorer
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe")));
    assert_eq!(
        local_drive_path(Path::new(r"\\?\C:\Downloads\test.txt")).unwrap(),
        Path::new(r"C:\Downloads\test.txt")
    );
    for invalid in [
        r"\\?\UNC\host\share\test.txt",
        r"\\host\share\test.txt",
        r"\\.\C:\test.txt",
        r"C:test.txt",
    ] {
        assert!(local_drive_path(Path::new(invalid)).is_err(), "{invalid}");
    }
}

#[test]
fn zone_identifier_marks_a_confined_file() {
    let temp = TempDir::new();
    let file = temp.0.join("torrent.bin");
    let stream = temp.0.join("torrent.bin:Zone.Identifier");
    fs::write(&file, b"test").unwrap();
    fs::write(
        &stream,
        b"[ZoneTransfer]\r\nZoneId=1\r\nHostUrl=https://example.com\r\n",
    )
    .unwrap();

    write_zone_identifier(&temp.0, "torrent.bin").unwrap();

    assert_eq!(fs::read(stream).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n");
    assert_eq!(fs::read(file).unwrap(), b"test");
}

#[test]
fn traversal_and_links_are_rejected_for_every_file_operation() {
    let temp = TempDir::new();
    let root = temp.0.join("downloads");
    let outside = temp.0.join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let protected = outside.join("secret.bin");
    fs::write(&protected, b"private").unwrap();
    let junction = root.join("junction");
    directory_junction(&junction, &outside);

    assert!(ensure_safe_directory(Path::new("relative/downloads")).is_err());
    assert!(ensure_safe_directory(&junction.join("missing_leaf")).is_err());
    assert!(!outside.join("missing_leaf").exists());

    let mut bad_paths = vec![
        "../outside/secret.bin",
        "junction/secret.bin",
        "junction/missing.bin",
        r"C:\Windows\file.txt",
        r"file.txt:stream",
        r"\\host\share\secret.bin",
    ];
    match std::os::windows::fs::symlink_file(&protected, root.join("symlink.bin")) {
        Ok(()) => bad_paths.push("symlink.bin"),
        Err(error) if error.raw_os_error() == Some(1314) => {
            // The junction still verifies reparse rejection on non-developer PCs.
        }
        Err(error) => panic!("{error}"),
    }
    for bad in bad_paths {
        assert!(reveal_action(&root, Some(bad)).is_err(), "{bad}");
        assert!(build_defender_scan_args(&root, bad).is_err(), "{bad}");
        assert!(hash_file(&root, bad).is_err(), "{bad}");
        assert!(delete_files(&root, &[bad.into()]).is_err(), "{bad}");
        assert!(write_zone_identifier(&root, bad).is_err(), "{bad}");
    }
    assert!(reveal_action(&junction, Some("secret.bin")).is_err());
    assert_eq!(fs::read(&protected).unwrap(), b"private");
    assert!(!outside.join("secret.bin:Zone.Identifier").exists());
}

#[test]
fn registers_firefox_only_and_chromium_in_an_isolated_key() {
    struct RegistryKey(String);
    impl Drop for RegistryKey {
        fn drop(&mut self) {
            let wide: Vec<_> = self.0.encode_utf16().chain([0]).collect();
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide.as_ptr()) };
        }
    }
    let temp = TempDir::new();
    let key = RegistryKey(format!(
        r"Software\SlipStreamDesktopTest-{}",
        Uuid::new_v4()
    ));
    let exe = env::current_exe().unwrap();
    assert!(!native_hosts_installed(&key.0));
    let firefox_path = temp
        .0
        .join("firefox")
        .join("com.ciphervaults.slipstream.json");
    let firefox =
        install_browser_hosts_into(&key.0, &temp.0, &exe, Some("  "), Some(""), Some("   "))
            .unwrap();
    assert_eq!(firefox.as_slice(), std::slice::from_ref(&firefox_path));
    assert!(native_hosts_installed(&key.0));
    let manifest: Value = serde_json::from_slice(&fs::read(&firefox_path).unwrap()).unwrap();
    assert_eq!(manifest["allowed_extensions"], json!([DEFAULT_FIREFOX_ID]));
    let executable = local_drive_path(&fs::canonicalize(&exe).unwrap()).unwrap();
    assert_eq!(manifest["path"], executable.to_str().unwrap());
    assert!(!temp.0.join("chrome").exists());
    fs::remove_file(&firefox_path).unwrap();
    assert!(!native_hosts_installed(&key.0));
    assert!(
        install_browser_hosts_into(&key.0, &temp.0, &exe, Some("invalid"), None, None).is_err()
    );
    assert!(
        install_browser_hosts_into(&key.0, &temp.0, &exe, None, None, Some("https://bad")).is_err()
    );

    let chrome = "abcdefghijklmnopabcdefghijklmnop";
    let edge = "chrome-extension://pppppppppppppppppppppppppppppppp/";
    let installed = install_browser_hosts_into(
        &key.0,
        &temp.0,
        &exe,
        Some(chrome),
        Some(edge),
        Some("test@example.org"),
    )
    .unwrap();
    let paths = ["chrome", "edge", "brave", "firefox"].map(|browser| {
        temp.0
            .join(browser)
            .join("com.ciphervaults.slipstream.json")
    });
    assert_eq!(installed, paths);
    assert!(native_hosts_installed(&key.0));
    for path in &paths[..3] {
        let manifest: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            manifest["allowed_origins"],
            json!([format!("chrome-extension://{chrome}/"), edge])
        );
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&paths[3]).unwrap()).unwrap();
    assert_eq!(manifest["allowed_extensions"], json!(["test@example.org"]));

    // Pointing the manifest to a nonexistent executable reports hosts as not installed.
    let mut bad_manifest = manifest;
    bad_manifest["path"] = json!(temp.0.join("missing.exe").to_string_lossy().into_owned());
    for path in &paths {
        fs::write(path, serde_json::to_vec(&bad_manifest).unwrap()).unwrap();
    }
    assert!(!native_hosts_installed(&key.0));
}

#[test]
fn settings_normalize_validates_paths_and_trims_tool_paths() {
    let temp = TempDir::new();
    let data = temp.0.join("data");
    let downloads = temp.0.join("dl");
    let outside = temp.0.join("outside");
    let junction = temp.0.join("junction");
    fs::create_dir(&data).unwrap();
    fs::create_dir(&outside).unwrap();
    directory_junction(&junction, &outside);

    let mut valid = AppSettings {
        default_download_dir: downloads.to_string_lossy().into_owned(),
        ytdlp_path: Some("   ".into()),
        ffmpeg_path: Some(" C:\\Tools\\ffmpeg.exe ".into()),
        ..AppSettings::default()
    };
    normalize(&mut valid, &data).unwrap();
    assert!(downloads.is_dir());
    assert_eq!(valid.ytdlp_path, None);
    assert_eq!(valid.ffmpeg_path.as_deref(), Some(r"C:\Tools\ffmpeg.exe"));

    for bad_dir in [
        "relative_dir".to_owned(),
        r"\\host\share\dl".to_owned(),
        junction.to_string_lossy().into_owned(),
        junction.join("sub").to_string_lossy().into_owned(),
    ] {
        let mut invalid = AppSettings {
            default_download_dir: bad_dir,
            ..AppSettings::default()
        };
        assert!(normalize(&mut invalid, &data).is_err());
    }
    assert!(!outside.join("sub").exists());

    for bad_tool in [
        "evil.exe",
        r"bin\evil.exe",
        r"\\server\share\evil.exe",
        r"C:\Tools\script.cmd",
        "bad\0.exe",
    ] {
        let mut invalid_tool = AppSettings {
            default_download_dir: downloads.to_string_lossy().into_owned(),
            ytdlp_path: Some(bad_tool.into()),
            ..AppSettings::default()
        };
        assert!(normalize(&mut invalid_tool, &data).is_err(), "{bad_tool}");
    }
    let junction_exe = junction.join("tool.exe");
    fs::write(outside.join("tool.exe"), b"exe").unwrap();
    let mut junction_tool = AppSettings {
        default_download_dir: downloads.to_string_lossy().into_owned(),
        ffmpeg_path: Some(junction_exe.to_string_lossy().into_owned()),
        ..AppSettings::default()
    };
    assert!(normalize(&mut junction_tool, &data).is_err());
    assert!(decode_sha256(&format!("  {}  \n", "a".repeat(64))).is_ok());
}

#[test]
fn toolchain_status_preserves_explicit_nulls() {
    let json = json!({
        "ytdlp_path": "C:\\Tools\\yt-dlp.exe",
        "ytdlp_version": "2026.10.01",
        "ffmpeg_path": null,
        "ffmpeg_version": null,
        "native_hosts_installed": true
    });
    let status: ToolchainStatus = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(status).unwrap(), json);
}

#[test]
fn expands_wildcards_and_clamps_only_torrent_limits() {
    assert_eq!(
        expand_wildcard_urls("https://cdn.example/file[07-09:2][a-b].mp4").unwrap(),
        [
            "https://cdn.example/file07a.mp4",
            "https://cdn.example/file07b.mp4",
            "https://cdn.example/file09a.mp4",
            "https://cdn.example/file09b.mp4",
        ]
    );
    assert!(expand_wildcard_urls("https://example.com/[9-2]").is_err());
    assert!(expand_wildcard_urls("file:///C:/test[1-4]").is_err());
    assert_eq!(torrent_rate(Some(0)), None);
    assert_eq!(torrent_rate(Some(1)), Some(16_384));
    assert_eq!(torrent_rate(Some(16_384)), Some(16_384));
    assert_eq!(torrent_rate(Some(128_000)), Some(128_000));
    assert_eq!(torrent_rate(Some(u64::MAX)), Some(u64::from(u32::MAX)));
}

#[test]
fn hardened_toolchain_rejects_junctions_and_ignores_junction_path_entries() {
    let temp = TempDir::new();
    let safe_dir = temp.0.join("safe");
    let outside_dir = temp.0.join("outside");
    let junction_dir = temp.0.join("junction");
    fs::create_dir(&safe_dir).unwrap();
    fs::create_dir(&outside_dir).unwrap();
    directory_junction(&junction_dir, &outside_dir);

    for dir in [&safe_dir, &outside_dir] {
        fs::write(dir.join("yt-dlp.exe"), b"exe").unwrap();
        fs::write(dir.join("ffmpeg.exe"), b"exe").unwrap();
    }

    let safe_path_env = env::join_paths([&junction_dir, &safe_dir]).unwrap();

    // Configured junction tool must fail immediately without falling back to PATH.
    let bad_configured = AppSettings {
        ytdlp_path: Some(
            junction_dir
                .join("yt-dlp.exe")
                .to_string_lossy()
                .into_owned(),
        ),
        ffmpeg_path: Some(safe_dir.join("ffmpeg.exe").to_string_lossy().into_owned()),
        ..AppSettings::default()
    };
    assert!(hardened_toolchain_config_with_path(&bad_configured, Some(&safe_path_env)).is_err());

    // PATH-only lookup with only a junction directory must fail.
    let junction_only_env = env::join_paths([&junction_dir]).unwrap();
    let auto_settings = AppSettings::default();
    assert!(hardened_toolchain_config_with_path(&auto_settings, Some(&junction_only_env)).is_err());

    // Mixed PATH skips the junction directory and resolves the canonical safe binaries.
    let resolved =
        hardened_toolchain_config_with_path(&auto_settings, Some(&safe_path_env)).unwrap();
    assert_eq!(
        resolved.ytdlp_path.unwrap(),
        fs::canonicalize(safe_dir.join("yt-dlp.exe")).unwrap()
    );
    assert_eq!(
        resolved.ffmpeg_path.unwrap(),
        fs::canonicalize(safe_dir.join("ffmpeg.exe")).unwrap()
    );
    assert_eq!(resolved.ffprobe_path, None);

    // An ffprobe.exe inside a junctioned PATH entry must also be rejected rather than passed as None.
    fs::write(outside_dir.join("ffprobe.exe"), b"exe").unwrap();
    assert!(hardened_toolchain_config_with_path(&auto_settings, Some(&safe_path_env)).is_err());
}
