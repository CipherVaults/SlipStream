use crate::{
    commands::friendly_updater_error,
    expand_wildcard_urls, install_browser_hosts_into,
    paths::{
        build_defender_scan_args, decode_sha256, delete_files, ensure_playlist_subfolder,
        ensure_safe_directory, ensure_safe_tree, hash_file, local_drive_path, reveal_action,
        with_completed_file, write_zone_identifier, RevealAction,
    },
    settings::{normalize, read_json, torrent_rate, write_json_atomically, SETTINGS_FILE},
    tools::{
        allowed_ytdlp_release_url, browser_native_host_registered, explorer_executable,
        hardened_toolchain_config_with_dirs, hardened_toolchain_config_with_path, list_browsers,
        native_hosts_installed, open_browser_extension_setup, parse_ffmpeg_version,
        parse_sha256sums_for_file, parse_ytdlp_version, prepare_browser_extension_setup,
        resolve_bundled_extension_dir_with_roots, resolve_toolchain_with_dirs,
        update_ytdlp_from_release, ytdlp_release_download_base,
    },
    ToolchainStatus,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slipstream_native_host::{
    manifest::{
        Browser, DEFAULT_CHROMIUM_EXTENSION_ID, DEFAULT_CHROMIUM_ORIGIN, DEFAULT_FIREFOX_ID,
    },
    registry::install_host,
};
use slipstream_protocol::{AppSettings, BrowserKind, ToolSource, TransferStatus, UpdateMode};
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
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};
use url::Url;
use uuid::Uuid;
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ,
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

struct RegistryKey(String);

#[test]
fn cache_paths_reject_descendant_junctions() {
    let temp = TempDir::new();
    let outside = TempDir::new();
    let cache = temp.0.join("yt-dlp-cache");
    let nested = cache.join("extractor");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("fixture.json"), b"{}").unwrap();
    assert!(ensure_safe_tree(&cache).is_ok());
    let link = nested.join("junction");
    directory_junction(&link, &outside.0);
    assert!(ensure_safe_tree(&cache).is_err());
    fs::remove_dir(link).unwrap();
}

impl Drop for RegistryKey {
    fn drop(&mut self) {
        let wide: Vec<_> = self.0.encode_utf16().chain([0]).collect();
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide.as_ptr()) };
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
fn playlist_subfolder_sanitizes_and_avoids_file_and_directory_collisions() {
    let temp = TempDir::new();
    let base = fs::canonicalize(&temp.0).unwrap();
    let sanitized = ensure_playlist_subfolder(&base, " Season 1: Intro / Preview? ").unwrap();
    assert_eq!(sanitized, base.join("Season 1_ Intro _ Preview_"));
    assert_eq!(
        ensure_playlist_subfolder(&base, "Season 1: Intro / Preview?").unwrap(),
        base.join("Season 1_ Intro _ Preview_ (2)")
    );
    assert_eq!(
        ensure_playlist_subfolder(&base, "").unwrap(),
        base.join("Playlist")
    );
    let long_name = ensure_playlist_subfolder(&base, &"あ".repeat(100)).unwrap();
    assert!(long_name.file_name().unwrap().to_str().unwrap().len() <= 120);

    fs::write(base.join("Season 2"), "existing file").unwrap();
    let suffixed = ensure_playlist_subfolder(&base, "Season 2").unwrap();
    assert_eq!(suffixed, base.join("Season 2 (2)"));
    assert_eq!(
        ensure_playlist_subfolder(&base, "Season 2").unwrap(),
        base.join("Season 2 (3)")
    );
    assert_eq!(
        fs::read_to_string(base.join("Season 2")).unwrap(),
        "existing file"
    );
}

#[test]
fn playlist_subfolder_rejects_traversal_devices_and_reparse_points() {
    let temp = TempDir::new();
    let outside = TempDir::new();
    directory_junction(&temp.0.join("Linked"), &outside.0);
    for bad in [
        "../Outside",
        "nested\\..\\Outside",
        "..",
        "CON.txt",
        "CONOUT$",
        "LPT\u{00b2}",
        "Linked",
    ] {
        let error = ensure_playlist_subfolder(&temp.0, bad).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{bad}");
    }
    assert!(fs::read_dir(&outside.0).unwrap().next().is_none());
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
        assert!(
            with_completed_file::<()>(TransferStatus::Completed, &root, bad, |_| panic!(
                "unsafe file opened"
            ))
            .is_err(),
            "{bad}"
        );
    }
    assert!(reveal_action(&junction, Some("secret.bin")).is_err());
    assert_eq!(fs::read(&protected).unwrap(), b"private");
    assert!(!outside.join("secret.bin:Zone.Identifier").exists());
}

#[test]
fn completed_file_open_requires_a_completed_regular_file() {
    let temp = TempDir::new();
    let path = temp.0.join("complete.bin");
    fs::write(&path, b"complete").unwrap();
    fs::create_dir(temp.0.join("directory")).unwrap();
    for (status, name) in [
        (TransferStatus::Paused, "complete.bin"),
        (TransferStatus::Completed, "missing.bin"),
        (TransferStatus::Completed, "directory"),
    ] {
        assert!(with_completed_file::<()>(status, &temp.0, name, |_| panic!(
            "invalid file opened"
        ))
        .is_err());
    }
    with_completed_file(
        TransferStatus::Completed,
        &temp.0,
        "complete.bin",
        |opened| {
            assert_eq!(opened, local_drive_path(&fs::canonicalize(&path)?).unwrap());
            assert_eq!(fs::read(opened)?, b"complete");
            Ok(())
        },
    )
    .unwrap();
}

#[test]
fn startup_migrates_legacy_hosts_and_setup_updates_only_the_selected_browser() {
    let temp = TempDir::new();
    let key = RegistryKey(format!(
        r"Software\SlipStreamDesktopTest-{}",
        Uuid::new_v4()
    ));
    let exe = temp.0.join("slipstream-native-host.exe");
    fs::File::create(&exe).unwrap();
    let canonical_exe = fs::canonicalize(&exe).unwrap();
    assert!(!native_hosts_installed(&key.0, &canonical_exe));
    let kinds = [
        BrowserKind::Chrome,
        BrowserKind::Edge,
        BrowserKind::Brave,
        BrowserKind::Firefox,
    ];
    for kind in kinds {
        assert!(!browser_native_host_registered(
            &key.0,
            kind,
            &canonical_exe
        ));
    }
    let legacy_origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop/";
    let legacy_firefox = "legacy@example.org";
    let old = temp.0.join("old");
    let old_chrome =
        install_host(&key.0, &old, Browser::Chrome, &exe, &[legacy_origin.into()]).unwrap();
    let read = |path: &Path| serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap();
    let mut old_manifest = read(&old_chrome);
    old_manifest["allowed_origins"] = json!([legacy_origin, "chrome-extension://*/"]);
    fs::write(&old_chrome, serde_json::to_vec(&old_manifest).unwrap()).unwrap();
    install_host(
        &key.0,
        &old,
        Browser::Firefox,
        &exe,
        &[legacy_firefox.into()],
    )
    .unwrap();
    assert!(!native_hosts_installed(&key.0, &canonical_exe));
    let paths = ["chrome", "edge", "brave", "firefox"].map(|browser| {
        temp.0
            .join(browser)
            .join("com.ciphervaults.slipstream.json")
    });
    let installed =
        install_browser_hosts_into(&key.0, &temp.0, &exe, Some("  "), Some(""), Some("   "))
            .unwrap();
    assert_eq!(installed, paths);
    assert!(native_hosts_installed(&key.0, &canonical_exe));
    let executable = local_drive_path(&canonical_exe).unwrap();
    let browsers = list_browsers(&key.0, Some(&temp.0));
    for ((kind, path), status) in kinds.into_iter().zip(&paths).zip(browsers) {
        assert!(browser_native_host_registered(&key.0, kind, &canonical_exe));
        assert_eq!(status.id, kind);
        assert!(status.native_host_registered);
        let manifest: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(manifest["path"], executable.to_str().unwrap());
        if kind == BrowserKind::Firefox {
            assert_eq!(status.extension_id, DEFAULT_FIREFOX_ID);
            assert_eq!(
                manifest["allowed_extensions"],
                json!([legacy_firefox, DEFAULT_FIREFOX_ID])
            );
        } else {
            assert_eq!(status.extension_id, DEFAULT_CHROMIUM_EXTENSION_ID);
            assert_eq!(
                manifest["allowed_origins"],
                if kind == BrowserKind::Chrome {
                    json!([legacy_origin, DEFAULT_CHROMIUM_ORIGIN])
                } else {
                    json!([DEFAULT_CHROMIUM_ORIGIN])
                }
            );
        }
    }
    fs::remove_file(&paths[3]).unwrap();
    assert!(!browser_native_host_registered(
        &key.0,
        BrowserKind::Firefox,
        &canonical_exe,
    ));
    assert!(native_hosts_installed(&key.0, &canonical_exe));
    assert!(
        install_browser_hosts_into(&key.0, &temp.0, &exe, Some("invalid"), None, None).is_err()
    );
    assert!(
        install_browser_hosts_into(&key.0, &temp.0, &exe, None, None, Some("https://bad")).is_err()
    );

    let extension = temp.0.join("extensions/chrome");
    fs::create_dir_all(extension.join("assets")).unwrap();
    fs::write(extension.join("manifest.json"), b"{}").unwrap();
    fs::write(extension.join("assets/script.js"), b"export {};").unwrap();
    let setup_dir = temp.0.join("setup");
    let untouched = [&paths[0], &paths[1], &paths[2]].map(|path| read(path));
    let edge = "chrome-extension://pppppppppppppppppppppppppppppppp/";
    prepare_browser_extension_setup(
        &key.0,
        &setup_dir,
        &extension,
        BrowserKind::Edge,
        Some(edge.into()),
        Some(&exe),
    )
    .unwrap();
    assert_eq!(fs::read_dir(&setup_dir).unwrap().count(), 1);
    assert_eq!(
        read(&setup_dir.join("edge/com.ciphervaults.slipstream.json"))["allowed_origins"],
        json!([DEFAULT_CHROMIUM_ORIGIN, edge])
    );
    assert_eq!(
        [&paths[0], &paths[1], &paths[2]].map(|path| read(path)),
        untouched
    );
    assert!(!browser_native_host_registered(
        &key.0,
        BrowserKind::Firefox,
        &canonical_exe
    ));

    let chrome = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let installed = install_browser_hosts_into(
        &key.0,
        &temp.0,
        &exe,
        Some(chrome),
        None,
        Some("test@example.org"),
    )
    .unwrap();
    assert_eq!(installed, paths);
    assert!(native_hosts_installed(&key.0, &canonical_exe));
    for (kind, path) in kinds.into_iter().zip(&paths[..3]) {
        let mut manifest: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            manifest["allowed_origins"],
            match kind {
                BrowserKind::Chrome => json!([
                    legacy_origin,
                    DEFAULT_CHROMIUM_ORIGIN,
                    format!("chrome-extension://{chrome}/")
                ]),
                BrowserKind::Edge => json!([DEFAULT_CHROMIUM_ORIGIN, edge]),
                _ => json!([DEFAULT_CHROMIUM_ORIGIN]),
            }
        );
        manifest["allowed_origins"] = json!(["chrome-extension://*/", "https://example.com"]);
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(!browser_native_host_registered(
            &key.0,
            kind,
            &canonical_exe
        ));
    }
    let mut manifest: Value = serde_json::from_slice(&fs::read(&paths[3]).unwrap()).unwrap();
    assert_eq!(
        manifest["allowed_extensions"],
        json!([DEFAULT_FIREFOX_ID, "test@example.org"])
    );
    assert!(browser_native_host_registered(
        &key.0,
        BrowserKind::Firefox,
        &canonical_exe
    ));
    manifest["allowed_extensions"] = json!(["test@example.org"]);
    fs::write(&paths[3], serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(!browser_native_host_registered(
        &key.0,
        BrowserKind::Firefox,
        &canonical_exe,
    ));

    manifest["allowed_extensions"] = json!([DEFAULT_FIREFOX_ID]);
    let unrelated = temp.0.join("unrelated.exe");
    fs::File::create(&unrelated).unwrap();
    manifest["path"] = json!(unrelated);
    fs::write(&paths[3], serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(!native_hosts_installed(&key.0, &canonical_exe));
    assert!(!list_browsers(&key.0, Some(&temp.0))[3].native_host_registered);
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
fn toolchain_status_defaults_new_fields_and_preserves_explicit_nulls() {
    let mut json = json!({
        "ytdlp_path": "C:\\Tools\\yt-dlp.exe",
        "ytdlp_version": "2026.10.01",
        "ffmpeg_path": null,
        "ffmpeg_version": null,
        "native_hosts_installed": true
    });
    let status: ToolchainStatus = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(status.ytdlp_source, ToolSource::Missing);
    assert_eq!(status.ffmpeg_source, ToolSource::Missing);
    assert_eq!(status.ffprobe_source, ToolSource::Missing);
    assert_eq!(status.ffprobe_path, None);
    assert_eq!(status.ffprobe_version, None);
    json["ytdlp_source"] = json!("missing");
    json["ffmpeg_source"] = json!("missing");
    json["ffprobe_source"] = json!("missing");
    json["ffprobe_path"] = Value::Null;
    json["ffprobe_version"] = Value::Null;
    assert_eq!(serde_json::to_value(&status).unwrap(), json);
    assert_eq!(
        serde_json::from_value::<ToolchainStatus>(json).unwrap(),
        status
    );
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

#[test]
fn resolves_toolchain_in_priority_order() {
    let temp = TempDir::new();
    let [custom, managed, bundled, path] =
        ["custom", "managed", "bundled", "path"].map(|name| temp.0.join(name));
    let bundled_dirs = [bundled.clone()];
    let names = ["yt-dlp.exe", "ffmpeg.exe", "ffprobe.exe"];
    for dir in [&custom, &managed, &bundled, &path] {
        fs::create_dir(dir).unwrap();
        for name in names {
            fs::write(dir.join(name), b"exe").unwrap();
        }
    }
    let path_env = env::join_paths([&path]).unwrap();
    let configured = AppSettings {
        ytdlp_path: Some(custom.join(names[0]).to_str().unwrap().to_owned()),
        ffmpeg_path: Some(custom.join(names[1]).to_str().unwrap().to_owned()),
        ..AppSettings::default()
    };
    let automatic = AppSettings::default();

    for (settings, managed_bin_dir, bundled_dirs, expected_dir, source) in [
        (
            &configured,
            Some(managed.as_path()),
            bundled_dirs.as_slice(),
            &custom,
            ToolSource::Custom,
        ),
        (
            &automatic,
            Some(managed.as_path()),
            bundled_dirs.as_slice(),
            &managed,
            ToolSource::Managed,
        ),
        (
            &automatic,
            None,
            bundled_dirs.as_slice(),
            &bundled,
            ToolSource::Bundled,
        ),
        (&automatic, None, &[], &path, ToolSource::SystemPath),
    ] {
        let result =
            resolve_toolchain_with_dirs(settings, managed_bin_dir, bundled_dirs, Some(&path_env));
        for (result, name) in [result.ytdlp, result.ffmpeg, result.ffprobe]
            .into_iter()
            .zip(names)
        {
            assert_eq!(
                result.unwrap().unwrap(),
                (fs::canonicalize(expected_dir.join(name)).unwrap(), source)
            );
        }
    }
    let result = resolve_toolchain_with_dirs(&automatic, None, &[], None);
    for tool in [result.ytdlp, result.ffmpeg, result.ffprobe] {
        assert_eq!(tool.unwrap(), None);
    }

    fs::remove_file(custom.join(names[2])).unwrap();
    let result =
        resolve_toolchain_with_dirs(&configured, Some(&managed), &bundled_dirs, Some(&path_env));
    assert_eq!(result.ffprobe.unwrap().unwrap().1, ToolSource::Managed);
    fs::create_dir(custom.join(names[2])).unwrap();
    let result =
        resolve_toolchain_with_dirs(&configured, Some(&managed), &bundled_dirs, Some(&path_env));
    assert!(result.ffprobe.is_err());
    assert!(hardened_toolchain_config_with_dirs(
        &configured,
        Some(&managed),
        &bundled_dirs,
        Some(&path_env)
    )
    .is_err());

    fs::remove_file(custom.join(names[0])).unwrap();
    fs::remove_file(custom.join(names[1])).unwrap();
    let result =
        resolve_toolchain_with_dirs(&configured, Some(&managed), &bundled_dirs, Some(&path_env));
    assert!(result.ytdlp.is_err());
    assert!(result.ffmpeg.is_err());
    assert!(result.ffprobe.is_err());

    let junction = temp.0.join("junction");
    directory_junction(&junction, &managed);
    let result =
        resolve_toolchain_with_dirs(&automatic, Some(&junction), &bundled_dirs, Some(&path_env));
    for tool in [result.ytdlp, result.ffmpeg, result.ffprobe] {
        assert_eq!(tool.unwrap().unwrap().1, ToolSource::Bundled);
    }
    let unsafe_custom = AppSettings {
        ytdlp_path: Some(junction.join(names[0]).to_str().unwrap().to_owned()),
        ..configured
    };
    let result = resolve_toolchain_with_dirs(
        &unsafe_custom,
        Some(&managed),
        &bundled_dirs,
        Some(&path_env),
    );
    assert!(result.ytdlp.is_err());
}

#[test]
fn parses_media_tool_versions() {
    for (raw, expected) in [
        (
            "\n \r\n  2026.10.26  stable \r\nother output",
            Some("2026.10.26"),
        ),
        ("2026.10.26.1 nightly", Some("2026.10.26.1")),
        (
            "yt-dlp fixture 1.0\nsecond line",
            Some("yt-dlp fixture 1.0"),
        ),
        ("20261.10.26 dev build", Some("20261.10.26 dev build")),
        ("\r\n  ", None),
    ] {
        assert_eq!(parse_ytdlp_version(raw).as_deref(), expected);
    }
    for (raw, expected) in [
        (
            "\nffmpeg version 8.0-full_build Copyright (c) 2000-2026 the FFmpeg developers",
            Some("8.0-full_build"),
        ),
        (
            " ffprobe version N-123456-gabc \r\nconfiguration: --enable-gpl",
            Some("N-123456-gabc"),
        ),
        (" ffmpeg fixture 1.0 ", Some("ffmpeg fixture 1.0")),
        ("", None),
    ] {
        assert_eq!(parse_ffmpeg_version(raw).as_deref(), expected);
    }
}

#[test]
fn settings_round_trip_update_mode_and_default_legacy_settings_to_auto() {
    let temp = TempDir::new();
    let settings_path = temp.0.join(SETTINGS_FILE);
    let mut legacy = serde_json::to_value(AppSettings::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("update_mode");
    write_json_atomically(&settings_path, &legacy).unwrap();
    assert_eq!(
        read_json::<AppSettings>(&settings_path)
            .unwrap()
            .update_mode,
        UpdateMode::Auto
    );

    for (wire, mode) in [("auto", UpdateMode::Auto), ("manual", UpdateMode::Manual)] {
        legacy["update_mode"] = json!(wire);
        write_json_atomically(&settings_path, &legacy).unwrap();
        let settings = read_json::<AppSettings>(&settings_path).unwrap();
        assert_eq!(settings.update_mode, mode);
        assert_eq!(serde_json::to_value(settings).unwrap()["update_mode"], wire);
    }
}

#[test]
fn updater_errors_omit_underlying_details() {
    for (error, expected) in [
        (
            "connection timed out for https://local.example",
            "Check your connection",
        ),
        (
            "404 Not Found https://local.example",
            "release could not be found",
        ),
        (
            "Signature verification failed: key contents",
            "signature could not be verified",
        ),
        (
            "expected value at line 1 column 2",
            "invalid release information",
        ),
        ("C:\\private\\path", "Try again later"),
    ] {
        let message = friendly_updater_error("check for updates", error);
        assert!(message.starts_with("Could not check for updates."));
        assert!(message.contains(expected));
        assert!(!message.contains(error));
    }
}

#[test]
fn finds_only_reparse_free_extension_directories_with_a_manifest() {
    let temp = TempDir::new();
    let exe = temp.0.join("desktop");
    let resource = temp.0.join("resource");
    let source = temp.0.join("source");
    let installed = exe.join("extensions/chrome");
    let packaged = resource.join("extensions/chrome");
    let nested = resource.join("resources/extensions/chrome");
    let development = source.join("extensions/browser/dist/chrome");
    for dir in [&installed, &packaged, &nested, &development] {
        fs::create_dir_all(dir).unwrap();
    }
    let resolve =
        |kind| resolve_bundled_extension_dir_with_roots(kind, Some(&exe), Some(&resource), &source);
    assert_eq!(resolve(BrowserKind::Chrome), None);
    fs::create_dir(installed.join("manifest.json")).unwrap();
    assert_eq!(resolve(BrowserKind::Chrome), None);
    fs::remove_dir(installed.join("manifest.json")).unwrap();
    fs::remove_dir(&installed).unwrap();

    fs::write(development.join("manifest.json"), b"{}").unwrap();
    let clean = |dir: &Path| local_drive_path(&fs::canonicalize(dir).unwrap()).unwrap();
    if cfg!(debug_assertions) {
        assert_eq!(resolve(BrowserKind::Brave), Some(clean(&development)));
    } else {
        assert_eq!(resolve(BrowserKind::Brave), None);
    }
    fs::write(nested.join("manifest.json"), b"{}").unwrap();
    assert_eq!(resolve(BrowserKind::Edge), Some(clean(&nested)));
    fs::write(packaged.join("manifest.json"), b"{}").unwrap();
    assert_eq!(resolve(BrowserKind::Chrome), Some(clean(&packaged)));
    directory_junction(&installed, &packaged);
    assert_eq!(resolve(BrowserKind::Chrome), Some(clean(&packaged)));
    fs::remove_dir(&installed).unwrap();
    fs::create_dir(&installed).unwrap();
    fs::write(installed.join("manifest.json"), b"{}").unwrap();
    assert_eq!(resolve(BrowserKind::Chrome), Some(clean(&installed)));

    assert_eq!(resolve(BrowserKind::Firefox), None);
    let firefox = resource.join("extensions/firefox");
    fs::create_dir(&firefox).unwrap();
    fs::write(firefox.join("manifest.json"), b"{}").unwrap();
    assert_eq!(resolve(BrowserKind::Firefox), Some(clean(&firefox)));
}

#[test]
fn browser_setup_rejects_invalid_custom_ids_before_registration() {
    let temp = TempDir::new();
    let key = RegistryKey(format!(
        r"Software\SlipStreamDesktopTest-{}",
        Uuid::new_v4()
    ));
    let manifests = temp.0.join("native-messaging");
    for (kind, id) in [(BrowserKind::Brave, " "), (BrowserKind::Firefox, "*")] {
        let error = open_browser_extension_setup(&key.0, &manifests, Some(&temp.0), kind, Some(id))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert!(!manifests.exists());
}

#[test]
fn nested_extension_reparse_points_are_rejected_before_host_registration() {
    let temp = TempDir::new();
    let key = RegistryKey(format!(
        r"Software\SlipStreamDesktopTest-{}",
        Uuid::new_v4()
    ));
    let exe = temp.0.join("slipstream-native-host.exe");
    fs::File::create(&exe).unwrap();
    let manifests = temp.0.join("native-messaging");
    let extension = temp.0.join("extensions/chrome");
    let assets = extension.join("assets");
    fs::create_dir_all(&assets).unwrap();
    fs::write(extension.join("manifest.json"), b"{}").unwrap();
    let external = temp.0.join("external");
    fs::create_dir(&external).unwrap();
    directory_junction(&assets.join("nested"), &external);
    let error = prepare_browser_extension_setup(
        &key.0,
        &manifests,
        &extension,
        BrowserKind::Chrome,
        None,
        Some(&exe),
    )
    .unwrap_err();
    assert!(error.to_string().contains("reparse point"), "{error}");
    assert!(!manifests.exists());
    assert!(!native_hosts_installed(
        &key.0,
        &fs::canonicalize(&exe).unwrap()
    ));
}

#[test]
fn yt_dlp_checksum_parser_accepts_common_formats_and_rejects_invalid_entries() {
    let hash = "ab".repeat(32);
    for listed in [
        "  yt-dlp.exe",
        " *yt-dlp.exe",
        "  ./yt-dlp.exe",
        " *./yt-dlp.exe",
    ] {
        assert_eq!(
            parse_sha256sums_for_file(&format!("{hash}{listed}\r\n"), "yt-dlp.exe").unwrap(),
            [0xab; 32]
        );
    }
    for contents in [
        format!("{hash}  yt-dlp.exe.zip"),
        format!("{hash}  beta/yt-dlp.exe"),
        "deadbeef  yt-dlp.exe".to_owned(),
        "*./yt-dlp.exe".to_owned(),
        format!("invalid *./yt-dlp.exe\n{hash}  yt-dlp.exe"),
        format!("{hash}  yt-dlp.exe\n{hash} *./yt-dlp.exe"),
        format!("{}  yt-dlp.exe", "zz".repeat(32)),
        format!("{hash}  yt-dlp.exe\n{}  yt-dlp.exe", "ff".repeat(32)),
    ] {
        assert!(parse_sha256sums_for_file(&contents, "yt-dlp.exe").is_err());
    }
}

#[test]
fn yt_dlp_latest_release_requires_a_concrete_tag_in_the_upstream_repository() {
    let url = Url::parse("https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26").unwrap();
    assert_eq!(
        ytdlp_release_download_base(&url).as_deref(),
        Some("https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26")
    );
    for url in [
        "https://github.com/yt-dlp/yt-dlp/releases/latest",
        "https://github.com/someone-else/yt-dlp/releases/tag/2026.10.26",
        "https://github.com/yt-dlp/yt-dlp/releases/tag/main/other",
        "https://github.com/yt-dlp/yt-dlp/releases/tag/2026%2f10%2f26",
        "https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26?other=tag",
        "https://release-assets.githubusercontent.com/yt-dlp/yt-dlp/releases/tag/2026.10.26",
    ] {
        assert!(
            ytdlp_release_download_base(&Url::parse(url).unwrap()).is_none(),
            "{url}"
        );
    }
}

#[test]
fn yt_dlp_release_policy_rejects_downgrades_and_untrusted_hosts() {
    for host in [
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com",
        "github-releases.githubusercontent.com",
    ] {
        let url = Url::parse(&format!(
            "https://{host}/release/yt-dlp.exe?signature=signed"
        ))
        .unwrap();
        assert!(allowed_ytdlp_release_url(&url));
    }
    for url in [
        "http://github.com/yt-dlp.exe",
        "https://github.com.attacker.example/yt-dlp.exe",
        "https://raw.githubusercontent.com/yt-dlp.exe",
        "https://github.com@attacker.example/yt-dlp.exe",
        "https://github.com:8443/yt-dlp.exe",
        "https://127.0.0.1/yt-dlp.exe",
    ] {
        assert!(!allowed_ytdlp_release_url(&Url::parse(url).unwrap()));
    }
}

async fn mock_ytdlp_release(responses: Vec<(&'static str, Vec<u8>)>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for (path, response) in responses {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(line, format!("GET /{path} HTTP/1.1\r\n"));
            loop {
                line.clear();
                if socket.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            socket.get_mut().write_all(&response).await.unwrap();
            socket.get_mut().shutdown().await.unwrap();
        }
    });
    (format!("http://{address}"), server)
}

fn release_response(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn ytdlp_test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn assert_no_ytdlp_staging_files(path: &Path) {
    assert!(!fs::read_dir(path).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".yt-dlp-")
    }));
}

#[tokio::test]
async fn yt_dlp_update_atomically_replaces_old_file_and_cleans_stale_downloads() {
    let temp = TempDir::new();
    let installed = temp.0.join("yt-dlp.exe");
    fs::write(&installed, b"old version").unwrap();
    let stale = fs::File::create(temp.0.join(".yt-dlp-stale.tmp")).unwrap();
    stale
        .set_modified(SystemTime::now() - Duration::from_secs(3601))
        .unwrap();
    drop(stale);
    let active = temp.0.join(".yt-dlp-active.tmp");
    fs::write(&active, b"active sibling download").unwrap();
    let payload = b"verified test payload";
    let sums = format!("{} *./yt-dlp.exe\n", hex::encode(Sha256::digest(payload)));
    let (base_url, server) = mock_ytdlp_release(vec![
        ("SHA2-256SUMS", release_response(sums.as_bytes())),
        ("yt-dlp.exe", release_response(payload)),
    ])
    .await;
    let (path, version) = update_ytdlp_from_release(Some(&base_url), &ytdlp_test_client(), &temp.0)
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(path, fs::canonicalize(&installed).unwrap());
    assert!(version.is_empty()); // The payload exercises installation without running an executable.
    assert_eq!(fs::read(installed).unwrap(), payload);
    assert!(active.is_file());
    fs::remove_file(active).unwrap();
    assert_no_ytdlp_staging_files(&temp.0);
}

#[tokio::test]
async fn yt_dlp_update_rejects_bad_or_missing_checksums_and_preserves_old_file() {
    let temp = TempDir::new();
    let installed = temp.0.join("yt-dlp.exe");
    fs::write(&installed, b"old version").unwrap();
    let mismatched = format!("{}  yt-dlp.exe\n", "00".repeat(32));
    let missing = format!("{}  yt-dlp_x86.exe\n", "00".repeat(32));
    for (sums, payload, error_text) in [
        (
            mismatched,
            Some(b"unexpected download".as_slice()),
            "failed SHA-256 verification",
        ),
        (missing, None, "checksum for yt-dlp.exe is missing"),
    ] {
        let mut responses = vec![("SHA2-256SUMS", release_response(sums.as_bytes()))];
        responses.extend(payload.map(|bytes| ("yt-dlp.exe", release_response(bytes))));
        let (base_url, server) = mock_ytdlp_release(responses).await;
        let error = update_ytdlp_from_release(Some(&base_url), &ytdlp_test_client(), &temp.0)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(error.contains(error_text), "{error}");
        assert_eq!(fs::read(&installed).unwrap(), b"old version");
        assert_no_ytdlp_staging_files(&temp.0);
    }
}

#[tokio::test]
async fn yt_dlp_update_rejects_http_errors_oversized_responses_and_interrupted_downloads() {
    let temp = TempDir::new();
    let bin = temp.0.join("bin");
    let payload = b"verified replacement";
    let sums = format!("{}  yt-dlp.exe", hex::encode(Sha256::digest(payload)));
    let oversized_sums = vec![b'#'; 1024 * 1024 + 1];
    let mut chunked = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
        oversized_sums.len()
    )
    .into_bytes();
    chunked.extend(oversized_sums);
    chunked.extend(b"\r\n0\r\n\r\n");
    let (base_url, server) = mock_ytdlp_release(vec![("SHA2-256SUMS", chunked)]).await;
    let error = update_ytdlp_from_release(Some(&base_url), &ytdlp_test_client(), &bin)
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(error.contains("SHA2-256SUMS exceeds"), "{error}");
    assert!(bin.is_dir());

    let installed = bin.join("yt-dlp.exe");
    fs::write(&installed, b"old version").unwrap();
    for (response, expected) in [
        (
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP 404",
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 134217729\r\nConnection: close\r\n\r\n",
            "yt-dlp.exe exceeds",
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 256\r\nConnection: close\r\n\r\npartial body",
            "download was interrupted",
        ),
    ] {
        let (base_url, server) = mock_ytdlp_release(vec![
            ("SHA2-256SUMS", release_response(sums.as_bytes())),
            ("yt-dlp.exe", response.as_bytes().to_vec()),
        ])
        .await;
        let error = update_ytdlp_from_release(Some(&base_url), &ytdlp_test_client(), &bin)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(error.contains(expected), "{error}");
        assert_eq!(fs::read(&installed).unwrap(), b"old version");
        assert_no_ytdlp_staging_files(&bin);
    }
}

#[tokio::test]
async fn yt_dlp_update_reports_a_locked_executable_and_removes_staging_file() {
    let temp = TempDir::new();
    let installed = temp.0.join("yt-dlp.exe");
    fs::write(&installed, b"old version").unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&installed)
        .unwrap();
    let payload = b"verified replacement";
    let sums = format!("{}  yt-dlp.exe", hex::encode(Sha256::digest(payload)));
    let (base_url, server) = mock_ytdlp_release(vec![
        ("SHA2-256SUMS", release_response(sums.as_bytes())),
        ("yt-dlp.exe", release_response(payload)),
    ])
    .await;
    let error = update_ytdlp_from_release(Some(&base_url), &ytdlp_test_client(), &temp.0)
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(
        error.contains("while a media download is using it"),
        "{error}"
    );
    drop(locked);
    assert_eq!(fs::read(&installed).unwrap(), b"old version");
    assert_no_ytdlp_staging_files(&temp.0);
}
