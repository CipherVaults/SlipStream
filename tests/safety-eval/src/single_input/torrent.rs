use super::{accepted, require, Suite, DEVICE_NAMES, SAFE_BASE, TITLES};
use slipstream_torrent::{
    bencode::{encode, BencodeValue as Value},
    build_v1_multi_file_torrent, build_v1_single_file_torrent, parse_torrent_bytes, TorrentError,
};
use std::{collections::BTreeMap, path::Path};

const CATEGORY: &str = "torrent_manifest";
const PIECE_LENGTH: i64 = 16 * 1024;
type Dict = BTreeMap<Vec<u8>, Value>;

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
    let base = Path::new(SAFE_BASE);
    let mut unsafe_names = vec![
        "../outside.bin".into(),
        "..".into(),
        "C:\\clip.mp4".into(),
        "/root.bin".into(),
        "\\root.bin".into(),
        "CON.bin".into(),
        "NUL".into(),
        "clip:ads".into(),
        "clip\u{202e}.bin".into(),
        "clip\u{2066}.bin".into(),
        "clip\u{2067}.bin".into(),
        "clip\u{2068}.bin".into(),
        "clip\u{2069}.bin".into(),
        "clip\0.bin".into(),
        format!("{}.bin", "界".repeat(66)),
    ];
    unsafe_names.extend(DEVICE_NAMES[4..].iter().map(|name| format!("{name}.bin")));
    for (location, offset) in [
        (Location::Name, 0),
        (Location::Utf8Name, 9),
        (Location::Path, 18),
        (Location::Utf8Path, 24),
        (Location::FileTree, 3),
    ] {
        let control = raw_manifest(location, None);
        for name in unsafe_names.iter().cycle().skip(offset).take(22) {
            reject_manifest(suite, base, location, &control, name);
        }
    }
    for location in [Location::V1Link, Location::V2Link] {
        let control = raw_manifest(location, None);
        for target in ["clip.bin", "..", "/root.bin", "C:\\clip.mp4", "clip:ads"] {
            reject_manifest(suite, base, location, &control, target);
        }
    }

    for title in TITLES {
        for suffix in ["txt", "bin", "final..pdf", "log", "json", "v2.mp4"] {
            let name = format!("{title}.{suffix}");
            let single = build_v1_single_file_torrent(&name, b"content", PIECE_LENGTH as u32);
            accept_manifest(suite, base, "v1_single_file", single, &[&name]);
            let nested = format!("資料 (archive)/{name}");
            let preview = format!("[preview]/🚀 {name}");
            let multi = build_v1_multi_file_torrent(
                title,
                &[(&nested, b"content"), (&preview, b"preview")],
                PIECE_LENGTH as u32,
            );
            accept_manifest(suite, base, "v1_multi_file", multi, &[&nested, &preview]);
        }
    }
}

fn reject_manifest(suite: &mut Suite, base: &Path, location: Location, control: &[u8], name: &str) {
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
    built: Result<Vec<u8>, TorrentError>,
    expected_paths: &[&str],
) {
    // The intended paths identify the input even if the builder regresses and emits no bytes.
    let input = expected_paths.join("\0").into_bytes();
    let item = suite.register(CATEGORY, "benign", surface, "composed", input);
    suite.grade(item, || {
        let bytes = accepted(built, "v1 torrent builder")?;
        let parsed = accepted(parse_torrent_bytes(&bytes, base), "parse_torrent_bytes")?;
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
        Ok("The torrent parser preserved every intended file under the save directory.")
    });
}

fn raw_manifest(location: Location, unsafe_name: Option<&str>) -> Vec<u8> {
    let mut info = BTreeMap::from([
        (b"piece length".to_vec(), Value::Int(PIECE_LENGTH)),
        (b"name".to_vec(), bytes("layout.bin")),
    ]);
    if matches!(location, Location::FileTree | Location::V2Link) {
        let mut properties = BTreeMap::from([(b"length".to_vec(), Value::Int(0))]);
        if let (Location::V2Link, Some(target)) = (location, unsafe_name) {
            add_symlink(&mut properties, target);
        }
        let name = if matches!(location, Location::FileTree) {
            unsafe_name
        } else {
            None
        };
        info.insert(b"meta version".to_vec(), Value::Int(2));
        info.insert(
            b"file tree".to_vec(),
            dictionary([(
                name.unwrap_or("file.bin").as_bytes(),
                dictionary([(b"", Value::Dict(properties))]),
            )]),
        );
        return encode(&dictionary([
            (b"info", Value::Dict(info)),
            (b"piece layers", Value::Dict(Dict::new())),
        ]));
    }

    info.insert(b"pieces".to_vec(), bytes(""));
    match location {
        Location::Name | Location::Utf8Name => {
            info.insert(b"length".to_vec(), Value::Int(0));
            let key: &[u8] = if matches!(location, Location::Name) {
                b"name"
            } else {
                b"name.utf-8"
            };
            info.insert(key.to_vec(), bytes(unsafe_name.unwrap_or("layout.bin")));
        }
        Location::Path | Location::Utf8Path | Location::V1Link => {
            let mut file = BTreeMap::from([
                (b"length".to_vec(), Value::Int(0)),
                (b"path".to_vec(), path("file.bin")),
            ]);
            if let Some(name) = unsafe_name {
                match location {
                    Location::Path => {
                        file.insert(b"path".to_vec(), path(name));
                    }
                    Location::Utf8Path => {
                        file.insert(b"path.utf-8".to_vec(), path(name));
                    }
                    Location::V1Link => add_symlink(&mut file, name),
                    _ => unreachable!(),
                }
            } else if matches!(location, Location::Utf8Path) {
                file.insert(b"path.utf-8".to_vec(), path("file.bin"));
            }
            info.insert(b"files".to_vec(), Value::List(vec![Value::Dict(file)]));
        }
        Location::FileTree | Location::V2Link => unreachable!(),
    }
    encode(&dictionary([(b"info", Value::Dict(info))]))
}

fn add_symlink(entry: &mut Dict, target: &str) {
    entry.insert(b"attr".to_vec(), bytes("l"));
    entry.insert(b"symlink path".to_vec(), path(target));
}

fn dictionary<const N: usize>(entries: [(&[u8], Value); N]) -> Value {
    Value::Dict(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_vec(), value))
            .collect(),
    )
}

fn bytes(text: &str) -> Value {
    Value::Bytes(text.as_bytes().to_vec())
}

fn path(component: &str) -> Value {
    Value::List(vec![bytes(component)])
}
