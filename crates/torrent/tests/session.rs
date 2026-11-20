use sha1::{Digest, Sha1};
use slipstream_protocol::{AddTorrentRequest, SegmentState, TransferKind, TransferStatus};
use slipstream_torrent::{
    bencode::{self, BencodeValue as V},
    build_v1_multi_file_torrent, build_v1_single_file_torrent, TorrentAddOptions, TorrentError,
    TorrentManager, TorrentManagerConfig,
};
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    time::Duration,
};
use tempfile::tempdir;

const PIECE: usize = 16 * 1024;
const WAIT: Duration = Duration::from_secs(15);

fn local_config(path: &Path) -> TorrentManagerConfig {
    TorrentManagerConfig {
        default_save_dir: path.to_owned(),
        enable_dht: false,
        enable_trackers: false,
        listen_port_range: None,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_bitmap_distinguishes_verified_and_corrupt_existing_pieces() {
    let temp = tempdir().unwrap();
    let mut payload = vec![1; PIECE];
    payload.extend(vec![2; PIECE]);
    let manifest = build_v1_single_file_torrent("partial.bin", &payload, PIECE as u32).unwrap();
    payload[PIECE..].fill(3);
    fs::write(temp.path().join("partial.bin"), &payload).unwrap();
    let manager = TorrentManager::new(local_config(temp.path()))
        .await
        .unwrap();
    let id = manager
        .add_torrent_bytes(
            &manifest,
            TorrentAddOptions {
                overwrite: true,
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager.wait_until_initialized(&id, WAIT).await.unwrap();
    let report = manager.stats(&id).unwrap();
    assert!(!report.finished);
    assert_eq!(report.snapshot.downloaded_bytes, PIECE as u64);
    assert_eq!(report.completed_pieces, 1);
    assert_eq!(&report.snapshot.ui_segments[..8], &[1.0; 8]);
    assert_eq!(&report.snapshot.ui_segments[8..], &[0.0; 8]);
    assert!(report.snapshot.segments[..8]
        .iter()
        .all(|seg| seg.state == SegmentState::Completed));
    assert!(report.snapshot.segments[8..]
        .iter()
        .all(|seg| seg.state == SegmentState::Paused));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_manifests_create_no_files() {
    let temp = tempdir().unwrap();
    let manager = TorrentManager::new(local_config(temp.path()))
        .await
        .unwrap();
    for segment in [
        b"../".as_slice(),
        b"foo/../../bar",
        b"/abs",
        b"\\abs",
        b"C:\\foo",
        b"CON",
        b"con.mp4",
        b"NUL.txt",
        b"COM1.exe",
        b"LPT9",
        b"file:stream",
        b"file.mp4:Zone.Identifier",
        b"file\0x",
        b"file\nx",
        b"trailing.",
        b"trailing ",
        b"",
        b".",
        b"..",
        b"\xff",
        "\u{202E}gpj.exe".as_bytes(),
        "a\u{202A}b".as_bytes(),
        "a\u{2069}b".as_bytes(),
        "a\u{061C}b".as_bytes(),
        "a\u{200E}b".as_bytes(),
        "a\u{200F}b".as_bytes(),
        "COM¹.mp4".as_bytes(),
    ] {
        let manifest = fixture(b"valid", &[b"folder", segment], None);
        assert!(
            matches!(
                manager
                    .add_torrent_bytes(&manifest, TorrentAddOptions::default())
                    .await,
                Err(TorrentError::PathSecurity(_))
            ),
            "{segment:?}"
        );
        assert!(
            matches!(
                manager
                    .add_torrent_bytes(
                        &fixture(segment, &[b"file"], None),
                        TorrentAddOptions::default()
                    )
                    .await,
                Err(TorrentError::PathSecurity(_))
            ),
            "root {segment:?}"
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    for manifest in [
        fixture(b"root", &[b"foo", b"..", b"..", b"bar"], None),
        fixture(b"root", &[], None),
        fixture(b"root", &[b"ln"], Some((b"attr", V::Bytes(b"hl".to_vec())))),
        fixture(
            b"root",
            &[b"ln"],
            Some((b"symlink path", V::List(vec![V::Bytes(b"target".to_vec())]))),
        ),
    ] {
        assert!(manager
            .add_torrent_bytes(&manifest, TorrentAddOptions::default())
            .await
            .is_err());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    let valid = build_v1_single_file_torrent("a.bin", b"test", PIECE as u32).unwrap();
    for bad_dir in ["../outside", "sub/CON", "sub/file:ads", "sub/\u{202E}bad"] {
        assert!(matches!(
            manager
                .add_torrent_bytes(
                    &valid,
                    TorrentAddOptions {
                        save_dir: Some(bad_dir.into()),
                        ..Default::default()
                    }
                )
                .await,
            Err(TorrentError::PathSecurity(_))
        ));
        assert!(matches!(
            manager
                .add_magnet(
                    &format!("magnet:?xt=urn:btih:{}", "cd".repeat(20)),
                    TorrentAddOptions {
                        save_dir: Some(bad_dir.into()),
                        ..Default::default()
                    }
                )
                .await,
            Err(TorrentError::PathSecurity(_))
        ));
    }
    let hostile_file = tempdir().unwrap();
    let hostile_torrent_path = hostile_file.path().join("evil.torrent");
    fs::write(
        &hostile_torrent_path,
        fixture(b"valid", &[b"sub", b"COM1.exe"], None),
    )
    .unwrap();
    assert!(matches!(
        manager
            .add_torrent(AddTorrentRequest {
                source: hostile_torrent_path.to_string_lossy().into_owned(),
                save_dir: None,
                selected_file_indices: None,
                sequential: false,
            })
            .await,
        Err(TorrentError::PathSecurity(_))
    ));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    assert!(manager.list_snapshots().is_empty());
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_torrent_from_disk_and_hybrid_metadata_and_empty_selection() {
    let temp = tempdir().unwrap();
    let src_dir = tempdir().unwrap();
    let manager = TorrentManager::new(local_config(temp.path()))
        .await
        .unwrap();

    // Hybrid v1+v2 torrent added via disk .torrent file path.
    let payload = b"abc";
    let v2_root: [u8; 32] = sha2::Sha256::digest(payload).into();
    let file_entry = dict([
        (b"length", V::Int(3)),
        (b"pieces root", V::Bytes(v2_root.to_vec())),
    ]);
    let info = dict([
        (b"file tree", dict([(b"a.txt", dict([(b"", file_entry)]))])),
        (b"length", V::Int(3)),
        (b"meta version", V::Int(2)),
        (b"name", V::Bytes(b"a.txt".to_vec())),
        (b"piece length", V::Int(16384)),
        (b"pieces", V::Bytes(Sha1::digest(payload).to_vec())),
    ]);
    let hybrid_bytes = bencode::encode(&dict([(b"info", info), (b"piece layers", dict([]))]));
    let torrent_path = src_dir.path().join("hybrid.torrent");
    fs::write(&torrent_path, &hybrid_bytes).unwrap();

    let id = manager
        .add_torrent(AddTorrentRequest {
            source: torrent_path.to_string_lossy().into_owned(),
            save_dir: Some("sub_folder".into()),
            selected_file_indices: None,
            sequential: true,
        })
        .await
        .unwrap();
    manager.wait_until_initialized(&id, WAIT).await.unwrap();
    let snap = manager.snapshot(&id).unwrap();
    assert!(snap.format_tags.iter().any(|t| t == "BTv1 + BTv2"));
    assert!(snap.format_tags.iter().any(|t| t == "sequential"));
    assert_eq!(snap.total_bytes, Some(3));

    // Empty file selection and re-selection should work cleanly without panicking.
    manager.set_selected_files(&id, &[]).await.unwrap();
    assert!(manager.list_files(&id).unwrap().iter().all(|f| !f.selected));
    assert_eq!(manager.snapshot(&id).unwrap().total_bytes, Some(0));
    manager.set_selected_files(&id, &[0]).await.unwrap();
    assert!(manager.list_files(&id).unwrap()[0].selected);
    assert_eq!(manager.snapshot(&id).unwrap().total_bytes, Some(3));

    manager.remove(&id, true).await.unwrap();
    manager.shutdown().await;
}

fn dict<const N: usize>(entries: [(&[u8], V); N]) -> V {
    V::Dict(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_vec(), value))
            .collect(),
    )
}

fn fixture(name: &[u8], paths: &[&[u8]], attr: Option<(&[u8], V)>) -> Vec<u8> {
    let mut file = std::collections::BTreeMap::from([
        (b"length".to_vec(), V::Int(4)),
        (
            b"path".to_vec(),
            V::List(paths.iter().map(|part| V::Bytes(part.to_vec())).collect()),
        ),
    ]);
    if let Some((key, attr)) = attr {
        file.insert(key.to_vec(), attr);
    }
    let info = V::Dict(std::collections::BTreeMap::from([
        (b"files".to_vec(), V::List(vec![V::Dict(file)])),
        (b"name".to_vec(), V::Bytes(name.to_vec())),
        (b"piece length".to_vec(), V::Int(PIECE as i64)),
        (b"pieces".to_vec(), V::Bytes(Sha1::digest(b"test").to_vec())),
    ]));
    bencode::encode(&V::Dict(std::collections::BTreeMap::from([(
        b"info".to_vec(),
        info,
    )])))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_magnets_are_registered_and_lifecycle_is_prompt() {
    let temp = tempdir().unwrap();
    let manager = TorrentManager::new(local_config(temp.path()))
        .await
        .unwrap();
    let source = format!("magnet:?xt=urn:btih:{}&dn=offline&xl=1234", "ab".repeat(20));
    let id = tokio::time::timeout(
        Duration::from_secs(1),
        manager.add_torrent(AddTorrentRequest {
            source,
            save_dir: None,
            selected_file_indices: None,
            sequential: true,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let snapshot = manager.snapshot(&id).unwrap();
    assert_eq!(snapshot.status, TransferStatus::Probing);
    assert_eq!(snapshot.total_bytes, Some(1234));
    assert_eq!(snapshot.filename, "offline");
    assert!(manager
        .wait_until_initialized(&id, Duration::from_millis(60))
        .await
        .is_err());
    manager.pause(&id).await.unwrap();
    assert_eq!(
        manager.snapshot(&id).unwrap().status,
        TransferStatus::Paused
    );
    manager.resume(&id).await.unwrap();
    assert_eq!(
        manager.snapshot(&id).unwrap().status,
        TransferStatus::Probing
    );
    manager.remove(&id, true).await.unwrap();
    assert!(matches!(manager.stats(&id), Err(TorrentError::NotFound(_))));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tcp_transfer_and_magnet_selection() {
    let seed_dir = tempdir().unwrap();
    let leech_dir = tempdir().unwrap();
    let files = [
        ("東京 (4K)/part one.bin", vec![30u8; 4 * PIECE]),
        (
            "مجلد/صور 🚀/part two.bin",
            (0..(19 * PIECE + 137)).map(|i| (i % 251) as u8).collect(),
        ),
    ];
    for (name, data) in &files {
        let path = seed_dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    let manifest = build_v1_multi_file_torrent("Collection 📦", &files, PIECE as u32).unwrap();
    let mut seed_config = local_config(seed_dir.path());
    seed_config.listen_port_range = Some(0..1);
    let seed = TorrentManager::new(seed_config).await.unwrap();
    let leech = TorrentManager::new(local_config(leech_dir.path()))
        .await
        .unwrap();
    let seed_id = seed
        .add_torrent_bytes(
            &manifest,
            TorrentAddOptions {
                overwrite: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    seed.wait_until_completed(&seed_id, WAIT).await.unwrap();
    let stats = seed.stats(&seed_id).unwrap();
    assert_eq!(stats.snapshot.status, TransferStatus::Seeding);
    assert_eq!(stats.snapshot.ui_segments, [1.0; 16]);
    assert_eq!(stats.completed_pieces, 24);
    let peer = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), seed.listen_port().unwrap());

    let id = leech
        .add_torrent_bytes(
            &manifest,
            TorrentAddOptions {
                start_paused: true,
                initial_peers: vec![peer],
                selected_file_indices: Some(vec![1, 0, 0]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    leech.wait_until_initialized(&id, WAIT).await.unwrap();
    assert_eq!(leech.snapshot(&id).unwrap().status, TransferStatus::Paused);
    leech.set_file_selected(&id, 0, false).await.unwrap();
    assert!(!leech.list_files(&id).unwrap()[0].selected);
    leech.set_file_selected(&id, 0, true).await.unwrap();
    assert!(leech
        .list_files(&id)
        .unwrap()
        .iter()
        .all(|file| file.selected));
    assert!(leech.set_selected_files(&id, &[99]).await.is_err());
    leech.set_selected_files(&id, &[0, 1]).await.unwrap();
    leech.set_sequential(&id, true).await.unwrap();
    leech
        .set_global_rate_limits(Some(16 * 1_048_576), None)
        .unwrap();
    leech
        .set_torrent_rate_limits(&id, Some(128 * 1024), Some(2 * 1_048_576))
        .await
        .unwrap();
    leech.resume(&id).await.unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let report = leech.stats(&id).unwrap();
            assert!(
                !report.finished,
                "expected telemetry before the throttled transfer finished"
            );
            if report.completed_pieces > 0 {
                assert_eq!(report.snapshot.status, TransferStatus::Active);
                assert!(report
                    .snapshot
                    .ui_segments
                    .iter()
                    .any(|progress| *progress > 0.0));
                assert!(report
                    .snapshot
                    .ui_segments
                    .iter()
                    .any(|progress| *progress < 1.0));
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    leech.wait_until_completed(&id, WAIT).await.unwrap();
    let report = leech.stats(&id).unwrap();
    assert_eq!(report.snapshot.kind, TransferKind::Torrent);
    assert_eq!(report.snapshot.ui_segments, [1.0; 16]);
    assert!(report
        .snapshot
        .segments
        .iter()
        .all(|seg| seg.state == SegmentState::Completed));
    assert_eq!(
        report.snapshot.downloaded_bytes,
        files.iter().map(|(_, data)| data.len() as u64).sum::<u64>()
    );
    assert_eq!(report.download_limit_bps, Some(128 * 1024));
    assert!(report.sequential);
    assert!(report
        .snapshot
        .format_tags
        .iter()
        .any(|tag| tag == "sequential"));
    assert!(report.seen_peers >= 1);
    for (name, data) in &files {
        assert_eq!(&fs::read(leech_dir.path().join(name)).unwrap(), data);
    }
    let uploaded = seed.stats(&seed_id).unwrap();
    assert!(uploaded.snapshot.uploaded_bytes > 0);
    assert!(uploaded.seen_peers >= 1);
    assert_eq!(
        uploaded.ratio,
        uploaded.snapshot.uploaded_bytes as f64 / uploaded.snapshot.downloaded_bytes as f64
    );
    seed.pause(&seed_id).await.unwrap();
    assert_eq!(
        seed.stats(&seed_id).unwrap().snapshot.status,
        TransferStatus::Paused
    );
    assert!(
        seed.stats(&seed_id).unwrap().snapshot.uploaded_bytes >= uploaded.snapshot.uploaded_bytes
    );
    seed.resume(&seed_id).await.unwrap();
    leech.remove(&id, true).await.unwrap();
    for (name, _) in &files {
        assert!(!leech_dir.path().join(name).exists());
    }

    let magnet = format!("magnet:?xt=urn:btih:{seed_id}&so=1");
    let id = leech
        .add_magnet(
            &magnet,
            TorrentAddOptions {
                initial_peers: vec![peer],
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(leech.snapshot(&id).unwrap().status, TransferStatus::Paused);
    assert_eq!(fs::read_dir(leech_dir.path()).unwrap().count(), 0);
    leech.resume(&id).await.unwrap();
    leech.wait_until_completed(&id, WAIT).await.unwrap();
    let report = leech.stats(&id).unwrap();
    assert_eq!(report.snapshot.status, TransferStatus::Completed);
    assert_eq!(report.snapshot.ui_segments[0], 0.0);
    assert_eq!(report.snapshot.ui_segments[15], 1.0);
    assert_eq!(report.snapshot.downloaded_bytes, files[1].1.len() as u64);
    assert_eq!(report.completed_pieces, 20);
    let listed = leech.list_files(&id).unwrap();
    assert!(!listed[0].selected);
    assert_eq!(listed[0].downloaded_bytes, 0);
    assert!(listed[1].selected);
    assert_eq!(
        &fs::read(leech_dir.path().join(files[1].0)).unwrap(),
        &files[1].1
    );
    assert_eq!(
        fs::metadata(leech_dir.path().join(files[0].0))
            .unwrap()
            .len(),
        0
    );
    leech.remove(&id, false).await.unwrap();
    assert!(leech_dir.path().join(files[1].0).is_file());
    tokio::join!(leech.shutdown(), seed.shutdown());
}
