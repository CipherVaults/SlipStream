use super::{accepted, require, Suite, DEVICE_NAMES, SAFE_BASE, TITLES};
use sha2::{Digest, Sha256};
use slipstream_torrent::{parse_torrent_bytes, TorrentError, TorrentVersion};
use std::{collections::BTreeMap, path::Path};

const CATEGORY: &str = "torrent_manifest";
const PIECE_LENGTH: usize = 16 * 1024;
type RawDict = BTreeMap<Vec<u8>, RawBencode>;

#[derive(Clone)]
enum RawBencode {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<RawBencode>),
    Dict(RawDict),
}

// Independent raw bencode encoder (intentionally decoupled from slipstream_torrent::bencode).
fn encode_raw(node: &RawBencode) -> Vec<u8> {
    fn write_bytes(bytes: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(bytes.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(bytes);
    }
    fn write(node: &RawBencode, out: &mut Vec<u8>) {
        match node {
            RawBencode::Int(v) => {
                out.push(b'i');
                out.extend_from_slice(v.to_string().as_bytes());
                out.push(b'e');
            }
            RawBencode::Bytes(bytes) => write_bytes(bytes, out),
            RawBencode::List(items) => {
                out.push(b'l');
                for item in items {
                    write(item, out);
                }
                out.push(b'e');
            }
            RawBencode::Dict(entries) => {
                out.push(b'd');
                for (k, v) in entries {
                    write_bytes(k, out);
                    write(v, out);
                }
                out.push(b'e');
            }
        }
    }
    let mut out = Vec::new();
    write(node, &mut out);
    out
}

// Independent pure-Rust FIPS 180-4 SHA-1 implementation for v1 piece hashing.
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let (chunks, rem) = msg.as_chunks::<64>();
    debug_assert!(rem.is_empty());
    for chunk in chunks {
        let mut w = [0u32; 80];
        let (words, word_rem) = chunk.as_chunks::<4>();
        debug_assert!(word_rem.is_empty());
        for (i, word) in words.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, h) in [h0, h1, h2, h3, h4].into_iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&h.to_be_bytes());
    }
    out
}

#[derive(Clone, Copy)]
enum Location {
    Name,
    Utf8Name,
    Path,
    Utf8Path,
    FileTree,
    V1Link,
    V2Link,
}

impl Location {
    fn surface(self) -> &'static str {
        match self {
            Self::Name => "v1_name",
            Self::Utf8Name => "v1_utf8_name",
            Self::Path => "v1_path",
            Self::Utf8Path => "v1_utf8_path",
            Self::FileTree => "v2_file_tree",
            Self::V1Link => "v1_symlink",
            Self::V2Link => "v2_symlink",
        }
    }

    fn is_symlink(self) -> bool {
        matches!(self, Self::V1Link | Self::V2Link)
    }
}

pub(super) fn run(suite: &mut Suite) {
    // Self-check independent SHA-1 against FIPS 180-4 test vector.
    assert_eq!(
        sha1_digest(b"abc"),
        [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ]
    );

    let base = Path::new(SAFE_BASE);
    let mut unsafe_names: Vec<Vec<u8>> = vec![
        b"../outside.bin".to_vec(),
        b"..".to_vec(),
        b"C:\\clip.mp4".to_vec(),
        b"/root.bin".to_vec(),
        b"\\root.bin".to_vec(),
        b"CON.bin".to_vec(),
        b"NUL".to_vec(),
        b"clip:ads".to_vec(),
        "clip\u{202e}.bin".as_bytes().to_vec(),
        "clip\u{2066}.bin".as_bytes().to_vec(),
        "clip\u{2067}.bin".as_bytes().to_vec(),
        "clip\u{2068}.bin".as_bytes().to_vec(),
        "clip\u{2069}.bin".as_bytes().to_vec(),
        b"clip\0.bin".to_vec(),
        format!("{}.bin", "界".repeat(66)).into_bytes(),
        // Invalid UTF-8 byte sequences alongside valid-UTF-8 traversal fixtures
        b"bad\xffname.bin".to_vec(),
        b"\x80invalid.mp4".to_vec(),
        b"trunc\xc3\x28.dat".to_vec(),
    ];
    unsafe_names.extend(
        DEVICE_NAMES[4..]
            .iter()
            .map(|name| format!("{name}.bin").into_bytes()),
    );
    for (location, offset) in [
        (Location::Name, 0),
        (Location::Utf8Name, 7),
        (Location::Path, 14),
        (Location::Utf8Path, 15),
        (Location::FileTree, 17),
    ] {
        let control = raw_manifest(location, None);
        for name in unsafe_names.iter().cycle().skip(offset).take(22) {
            reject_manifest(suite, base, location, &control, name);
        }
    }
    for location in [Location::V1Link, Location::V2Link] {
        let control = raw_manifest(location, None);
        for target in ["clip.bin", "..", "/root.bin", "C:\\clip.mp4", "clip:ads"] {
            reject_manifest(suite, base, location, &control, target.as_bytes());
        }
    }

    // 40 independently encoded benign v1 single-file Unicode manifests
    for (t_idx, title) in TITLES.iter().enumerate() {
        for (s_idx, suffix) in ["txt", "bin", "final..pdf", "v2.mp4"].iter().enumerate() {
            let name = format!("{title}.{suffix}");
            let payload = format!("v1-single-{t_idx}-{s_idx}-{title}").into_bytes();
            let raw = encode_independent_v1_single(&name, &payload);
            accept_manifest(
                suite,
                base,
                "v1_single_file",
                TorrentVersion::V1,
                raw,
                &[&name],
            );
        }
    }

    // 40 independently encoded benign v1 multi-file Unicode manifests
    for (t_idx, title) in TITLES.iter().enumerate() {
        for (s_idx, suffix) in ["txt", "bin", "final..pdf", "json"].iter().enumerate() {
            let name = format!("{title}.{suffix}");
            let nested = format!("資料 (archive)/{name}");
            let preview = format!("[preview]/🚀 {s_idx}_{name}");
            let p1 = format!("v1-multi-a-{t_idx}-{s_idx}").into_bytes();
            let p2 = format!("v1-multi-b-{t_idx}-{s_idx}").into_bytes();
            let raw = encode_independent_v1_multi(title, &[(&nested, &p1), (&preview, &p2)]);
            accept_manifest(
                suite,
                base,
                "v1_multi_file",
                TorrentVersion::V1,
                raw,
                &[&nested, &preview],
            );
        }
    }

    // 40 independently encoded benign v2 Unicode manifests (20 single-block + 20 multi-piece Merkle piece layers)
    for (t_idx, title) in TITLES.iter().enumerate() {
        for (s_idx, suffix) in ["mkv", "flac"].iter().enumerate() {
            let name = format!("{title}_v2.{suffix}");
            let small_payload = format!("v2-small-{t_idx}-{s_idx}-{title}").into_bytes();
            let raw_small = encode_independent_v2(&[(vec![name.as_str()], &small_payload)], title);
            accept_manifest(
                suite,
                base,
                "v2_single_file",
                TorrentVersion::V2,
                raw_small,
                &[&name],
            );

            // Multi-piece (> 16 KiB) file exercising v2 Merkle piece layer verification
            let folder = "東京 (4K)";
            let multi_name = format!("{title}_{s_idx}.dat");
            let mut large_payload =
                vec![((t_idx * 17 + s_idx + 1) & 0xff) as u8; PIECE_LENGTH + 512];
            large_payload[0] = b'S';
            large_payload[PIECE_LENGTH] = b'L';
            let expected_rel = format!("{folder}/{multi_name}");
            let raw_multi = encode_independent_v2(
                &[
                    (vec![folder, multi_name.as_str()], &large_payload),
                    (vec!["ملف_مرافق.txt"], small_payload.as_slice()),
                ],
                title,
            );
            accept_manifest(
                suite,
                base,
                "v2_multi_file",
                TorrentVersion::V2,
                raw_multi,
                &["ملف_مرافق.txt", &expected_rel],
            );
        }
    }
}

fn encode_independent_v1_single(name: &str, data: &[u8]) -> Vec<u8> {
    let mut pieces = Vec::new();
    for chunk in data.chunks(PIECE_LENGTH) {
        pieces.extend_from_slice(&sha1_digest(chunk));
    }
    let info = raw_dict([
        (b"length", RawBencode::Int(data.len() as i64)),
        (b"name", RawBencode::Bytes(name.as_bytes().to_vec())),
        (b"piece length", RawBencode::Int(PIECE_LENGTH as i64)),
        (b"pieces", RawBencode::Bytes(pieces)),
    ]);
    encode_raw(&raw_dict([(b"info", info)]))
}

fn encode_independent_v1_multi(name: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut concatenated = Vec::new();
    let mut file_entries = Vec::new();
    for &(rel_path, data) in files {
        concatenated.extend_from_slice(data);
        let path_list: Vec<_> = rel_path
            .split('/')
            .map(|part| RawBencode::Bytes(part.as_bytes().to_vec()))
            .collect();
        file_entries.push(raw_dict([
            (b"length", RawBencode::Int(data.len() as i64)),
            (b"path", RawBencode::List(path_list)),
        ]));
    }
    let mut pieces = Vec::new();
    for chunk in concatenated.chunks(PIECE_LENGTH) {
        pieces.extend_from_slice(&sha1_digest(chunk));
    }
    let info = raw_dict([
        (b"files", RawBencode::List(file_entries)),
        (b"name", RawBencode::Bytes(name.as_bytes().to_vec())),
        (b"piece length", RawBencode::Int(PIECE_LENGTH as i64)),
        (b"pieces", RawBencode::Bytes(pieces)),
    ]);
    encode_raw(&raw_dict([(b"info", info)]))
}

fn encode_independent_v2(files: &[(Vec<&str>, &[u8])], root_name: &str) -> Vec<u8> {
    let mut tree = RawDict::new();
    let mut piece_layers = RawDict::new();
    for (components, data) in files {
        let leaf_hashes: Vec<[u8; 32]> = data
            .chunks(PIECE_LENGTH)
            .map(|chunk| Sha256::digest(chunk).into())
            .collect();
        let pieces_root = merkle_root_v2(&leaf_hashes);
        if data.len() > PIECE_LENGTH {
            let flattened: Vec<u8> = leaf_hashes.iter().flat_map(|h| h.iter().copied()).collect();
            piece_layers.insert(pieces_root.to_vec(), RawBencode::Bytes(flattened));
        }
        let leaf_dict = raw_dict([
            (b"length", RawBencode::Int(data.len() as i64)),
            (b"pieces root", RawBencode::Bytes(pieces_root.to_vec())),
        ]);
        insert_tree_file(&mut tree, components, leaf_dict);
    }
    let info = raw_dict([
        (b"file tree", RawBencode::Dict(tree)),
        (b"meta version", RawBencode::Int(2)),
        (b"name", RawBencode::Bytes(root_name.as_bytes().to_vec())),
        (b"piece length", RawBencode::Int(PIECE_LENGTH as i64)),
    ]);
    encode_raw(&raw_dict([
        (b"info", info),
        (b"piece layers", RawBencode::Dict(piece_layers)),
    ]))
}

fn merkle_root_v2(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    let mut level = leaves.to_vec();
    let mut pad = [0u8; 32];
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let right = pair.get(1).unwrap_or(&pad);
            let mut h = Sha256::new();
            h.update(pair[0]);
            h.update(right);
            next.push(h.finalize().into());
        }
        level = next;
        let mut ph = Sha256::new();
        ph.update(pad);
        ph.update(pad);
        pad = ph.finalize().into();
    }
    level[0]
}

fn insert_tree_file(tree: &mut RawDict, components: &[&str], leaf: RawBencode) {
    if let Some((head, tail)) = components.split_first() {
        if tail.is_empty() {
            let entry = raw_dict([(b"", leaf)]);
            tree.insert(head.as_bytes().to_vec(), entry);
        } else {
            let child = tree
                .entry(head.as_bytes().to_vec())
                .or_insert_with(|| RawBencode::Dict(RawDict::new()));
            let RawBencode::Dict(sub) = child else {
                panic!("directory collides with file");
            };
            insert_tree_file(sub, tail, leaf);
        }
    }
}

fn reject_manifest(
    suite: &mut Suite,
    base: &Path,
    location: Location,
    control: &[u8],
    name: &[u8],
) {
    let input = raw_manifest(location, Some(name));
    let item = suite.register(
        CATEGORY,
        "harmful",
        location.surface(),
        "composed",
        input.clone(),
    );
    suite.grade(item, || {
        // Confirm that an otherwise identical layout reaches the path/attribute checks.
        parse_torrent_bytes(control, base).expect("invalid torrent control fixture");
        let result = parse_torrent_bytes(&input, base);
        let blocked = if location.is_symlink() {
            matches!(result, Err(TorrentError::Unsupported(ref why)) if why.contains("symlink"))
        } else {
            matches!(result, Err(TorrentError::PathSecurity(_)))
        };
        require(
            blocked,
            format!("expected a path or symlink rejection, got {result:?}"),
        )?;
        Ok("The torrent parser rejected the unsafe manifest path or symlink.")
    });
}

fn accept_manifest(
    suite: &mut Suite,
    base: &Path,
    surface: &'static str,
    expected_version: TorrentVersion,
    bytes: Vec<u8>,
    expected_paths: &[&str],
) {
    // Deduplicate directly on the independently encoded manifest bytes.
    let item = suite.register(CATEGORY, "benign", surface, "composed", bytes.clone());
    suite.grade(item, || {
        let parsed = accepted(parse_torrent_bytes(&bytes, base), "parse_torrent_bytes")?;
        require(
            parsed.version == expected_version,
            format!(
                "expected torrent version {expected_version:?}, got {:?}",
                parsed.version
            ),
        )?;
        let expected: Vec<_> = expected_paths
            .iter()
            .map(|path| {
                path.split('/')
                    .fold(base.to_path_buf(), |prefix, component| {
                        prefix.join(component)
                    })
            })
            .collect();
        require(
            parsed.resolved_paths == expected
                && parsed
                    .resolved_paths
                    .iter()
                    .all(|path| path.starts_with(base)),
            "the torrent did not resolve all intended files below the save directory",
        )?;
        Ok("The torrent parser preserved every independently encoded file under the save directory.")
    });
}

fn raw_manifest(location: Location, unsafe_name: Option<&[u8]>) -> Vec<u8> {
    let mut info = BTreeMap::from([
        (
            b"piece length".to_vec(),
            RawBencode::Int(PIECE_LENGTH as i64),
        ),
        (b"name".to_vec(), RawBencode::Bytes(b"layout.bin".to_vec())),
    ]);
    if matches!(location, Location::FileTree | Location::V2Link) {
        let mut properties = BTreeMap::from([(b"length".to_vec(), RawBencode::Int(0))]);
        if let (Location::V2Link, Some(target)) = (location, unsafe_name) {
            add_symlink(&mut properties, target);
        }
        let name = if matches!(location, Location::FileTree) {
            unsafe_name
        } else {
            None
        };
        info.insert(b"meta version".to_vec(), RawBencode::Int(2));
        info.insert(
            b"file tree".to_vec(),
            raw_dict([(
                name.unwrap_or(b"file.bin"),
                raw_dict([(b"", RawBencode::Dict(properties))]),
            )]),
        );
        return encode_raw(&raw_dict([
            (b"info", RawBencode::Dict(info)),
            (b"piece layers", RawBencode::Dict(RawDict::new())),
        ]));
    }

    info.insert(b"pieces".to_vec(), RawBencode::Bytes(Vec::new()));
    match location {
        Location::Name | Location::Utf8Name => {
            info.insert(b"length".to_vec(), RawBencode::Int(0));
            let key: &[u8] = if matches!(location, Location::Name) {
                b"name"
            } else {
                b"name.utf-8"
            };
            info.insert(
                key.to_vec(),
                RawBencode::Bytes(unsafe_name.unwrap_or(b"layout.bin").to_vec()),
            );
        }
        Location::Path | Location::Utf8Path | Location::V1Link => {
            let mut file = BTreeMap::from([
                (b"length".to_vec(), RawBencode::Int(0)),
                (b"path".to_vec(), path_bytes(b"file.bin")),
            ]);
            if let Some(name) = unsafe_name {
                match location {
                    Location::Path => {
                        file.insert(b"path".to_vec(), path_bytes(name));
                    }
                    Location::Utf8Path => {
                        file.insert(b"path.utf-8".to_vec(), path_bytes(name));
                    }
                    Location::V1Link => add_symlink(&mut file, name),
                    _ => unreachable!(),
                }
            } else if matches!(location, Location::Utf8Path) {
                file.insert(b"path.utf-8".to_vec(), path_bytes(b"file.bin"));
            }
            info.insert(
                b"files".to_vec(),
                RawBencode::List(vec![RawBencode::Dict(file)]),
            );
        }
        Location::FileTree | Location::V2Link => unreachable!(),
    }
    encode_raw(&raw_dict([(b"info", RawBencode::Dict(info))]))
}

fn add_symlink(entry: &mut RawDict, target: &[u8]) {
    entry.insert(b"attr".to_vec(), RawBencode::Bytes(b"l".to_vec()));
    entry.insert(b"symlink path".to_vec(), path_bytes(target));
}

fn raw_dict<const N: usize>(entries: [(&[u8], RawBencode); N]) -> RawBencode {
    RawBencode::Dict(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_vec(), value))
            .collect(),
    )
}

fn path_bytes(component: &[u8]) -> RawBencode {
    RawBencode::List(vec![RawBencode::Bytes(component.to_vec())])
}
