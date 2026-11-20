use sha1::{Digest, Sha1};
use sha2::Sha256;
use slipstream_torrent::{
    bencode::{self, BencodeValue as V},
    build_v1_multi_file_torrent, build_v1_single_file_torrent, parse_torrent_bytes, TorrentVersion,
};
use std::path::Path;

fn dict<const N: usize>(entries: [(&[u8], V); N]) -> V {
    V::Dict(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_vec(), value))
            .collect(),
    )
}

fn bytes(bytes: impl AsRef<[u8]>) -> V {
    V::Bytes(bytes.as_ref().to_owned())
}

#[test]
fn v1_hashes_raw_info_and_builds_cross_file_pieces() {
    let raw_info = b"d4:name5:a.txt6:lengthi0e12:piece lengthi16384e6:pieces0:e";
    let encoded = [b"d4:info".as_slice(), raw_info, b"e"].concat();
    let parsed = parse_torrent_bytes(&encoded, Path::new("download")).unwrap();
    assert_eq!(parsed.version, TorrentVersion::V1);
    assert_eq!(parsed.info_hash_v1, Some(Sha1::digest(raw_info).into()));
    assert_eq!(parsed.total_pieces, 0);

    let single = build_v1_single_file_torrent("hello.bin", b"abcdef", 4).unwrap();
    let parsed = parse_torrent_bytes(&single, Path::new("download")).unwrap();
    assert_eq!(parsed.files[0].size_bytes, 6);
    assert_eq!(
        parsed.pieces,
        [
            <[u8; 20]>::from(Sha1::digest(b"abcd")),
            Sha1::digest(b"ef").into()
        ]
    );

    let files = [
        ("العربية/東京 (4K)/🚀.bin", b"ab".as_slice()),
        ("zweite Datei.txt", b"cdef".as_slice()),
    ];
    let multi = build_v1_multi_file_torrent("ألبوم الصور 📸", &files, 4).unwrap();
    let parsed_multi = parse_torrent_bytes(&multi, Path::new("download")).unwrap();
    assert_eq!(parsed_multi.files[0].relative_path, files[0].0);
    assert_eq!(
        parsed_multi.resolved_paths[0],
        Path::new("download").join(files[0].0)
    );
    assert_eq!(parsed_multi.pieces, parsed.pieces);
    assert_eq!(parsed_multi.total_bytes, 6);
}

#[test]
fn parses_v2_single_file_and_validates_multifile_merkle_layer() {
    let payload = b"abc";
    let leaf = Sha256::digest(payload);
    let single_leaf = dict([(b"length", V::Int(3)), (b"pieces root", bytes(leaf))]);
    let info = dict([
        (
            b"file tree",
            dict([(b"a.txt", dict([(b"", single_leaf.clone())]))]),
        ),
        (b"meta version", V::Int(2)),
        (b"name", bytes("Tokyo 東京")),
        (b"piece length", V::Int(16384)),
        (b"private", V::Int(1)),
    ]);
    let root = dict([
        (b"announce", bytes("udp://tracker.test:50")),
        (
            b"announce-list",
            V::List(vec![V::List(vec![bytes("udp://tracker.test:50")])]),
        ),
        (b"comment", bytes("fixture")),
        (b"created by", bytes("test")),
        (b"info", info.clone()),
        (b"piece layers", dict([])),
    ]);
    let parsed = parse_torrent_bytes(&bencode::encode(&root), Path::new("dest")).unwrap();
    assert_eq!(parsed.version, TorrentVersion::V2);
    assert_eq!(parsed.info_hash_v1, None);
    assert_eq!(
        parsed.info_hash_v2,
        Some(Sha256::digest(bencode::encode(&info)).into())
    );
    assert_eq!(parsed.file_specs[0].pieces_root, Some(leaf.into()));
    assert_eq!(parsed.announce_list, [["udp://tracker.test:50"]]);
    assert_eq!(parsed.announce.as_deref(), Some("udp://tracker.test:50"));
    assert_eq!(parsed.comment.as_deref(), Some("fixture"));
    assert_eq!(parsed.created_by.as_deref(), Some("test"));
    assert!(parsed.private);

    // Three leaf hashes require one zero hash to complete the tree.
    let pair = |left: &[u8], right: &[u8]| Sha256::digest([left, right].concat());
    let pieces_root = pair(&pair(&[1; 32], &[2; 32]), &pair(&[3; 32], &[0; 32]));
    let large = dict([
        (b"length", V::Int(32769)),
        (b"pieces root", bytes(pieces_root)),
    ]);
    let info = dict([
        (
            b"file tree",
            dict([
                (
                    "日本".as_bytes(),
                    dict([(b"a.txt", dict([(b"", single_leaf)]))]),
                ),
                (
                    "عربى".as_bytes(),
                    dict([("🚀 (1).bin".as_bytes(), dict([(b"", large)]))]),
                ),
            ]),
        ),
        (b"meta version", V::Int(2)),
        (b"piece length", V::Int(16384)),
    ]);
    let valid_hashes = [[1; 32], [2; 32], [3; 32]].concat();
    let manifest = |hashes| {
        bencode::encode(&dict([
            (b"info", info.clone()),
            (b"piece layers", dict([(&pieces_root, bytes(hashes))])),
        ]))
    };
    let parsed = parse_torrent_bytes(&manifest(valid_hashes.clone()), Path::new("dest")).unwrap();
    assert_eq!(parsed.total_pieces, 4);
    assert_eq!(parsed.total_bytes, 32772);
    assert_eq!(parsed.files.len(), 2);
    assert_eq!(
        parsed
            .piece_layers
            .get(&<[u8; 32]>::from(pieces_root))
            .unwrap()
            .len(),
        3
    );
    let mut corrupt = valid_hashes;
    corrupt[0] ^= 1;
    assert!(parse_torrent_bytes(&manifest(corrupt), Path::new("dest")).is_err());
}

#[test]
fn parses_hybrid_and_hashes_both_versions_of_the_same_info_bytes() {
    let file = dict([
        (b"length", V::Int(3)),
        (b"pieces root", bytes(Sha256::digest(b"abc"))),
    ]);
    let info = dict([
        (b"file tree", dict([(b"a.txt", dict([(b"", file)]))])),
        (b"length", V::Int(3)),
        (b"meta version", V::Int(2)),
        (b"name", bytes("a.txt")),
        (b"piece length", V::Int(16384)),
        (b"pieces", bytes(Sha1::digest(b"abc"))),
    ]);
    let data = bencode::encode(&dict([
        (b"info", info.clone()),
        (b"piece layers", dict([])),
    ]));
    let parsed = parse_torrent_bytes(&data, Path::new("dest")).unwrap();
    let raw = bencode::extract_info_raw_bytes(&data).unwrap();
    assert_eq!(parsed.version, TorrentVersion::Hybrid);
    assert_eq!(parsed.info_hash_v1, Some(Sha1::digest(raw).into()));
    assert_eq!(parsed.info_hash_v2, Some(Sha256::digest(raw).into()));
    assert!(parse_torrent_bytes(
        &bencode::encode(&dict([(b"info", info)])),
        Path::new("dest")
    )
    .is_err());
}
