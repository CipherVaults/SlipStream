use crate::{bencode, bencode::BencodeValue as Value, magnet::allowed_tracker_url, TorrentError};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use slipstream_protocol::{
    safe_join_under, validate_safe_filename_component, PathSecurityError, TorrentFileEntry,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

type Dict = BTreeMap<Vec<u8>, Value>;
const V2_BLOCK_SIZE: u64 = 16 * 1024;
pub const MAX_TORRENT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentVersion {
    V1,
    V2,
    Hybrid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFileSpec {
    pub relative_path: String,
    pub size_bytes: u64,
    pub is_padding: bool,
    pub pieces_root: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
pub struct TorrentMetadata {
    pub version: TorrentVersion,
    pub info_hash_v1: Option<[u8; 20]>,
    pub info_hash_v2: Option<[u8; 32]>,
    pub name: Option<String>,
    pub piece_length: u64,
    /// Size of the file layout, including explicit v1 padding, if present.
    pub total_bytes: u64,
    pub total_pieces: usize,
    pub private: bool,
    pub announce: Option<String>,
    pub announce_list: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub pieces: Vec<[u8; 20]>,
    pub piece_layers: BTreeMap<[u8; 32], Vec<[u8; 32]>>,
    /// In v1 wire order (including padding) or v2 tree order for a v2-only torrent.
    pub file_specs: Vec<TorrentFileSpec>,
    pub files: Vec<TorrentFileEntry>,
    pub resolved_paths: Vec<PathBuf>,
}

pub fn parse_torrent_bytes(bytes: &[u8], save_dir: &Path) -> Result<TorrentMetadata, TorrentError> {
    parse_metadata(bytes, save_dir, false)
}

/// BEP 9 sends only the info dictionary. A BTv1 transfer of a hybrid torrent can
/// verify the SHA-1 pieces without the outer BTv2 piece layers.
pub(crate) fn parse_wire_metadata(
    bytes: &[u8],
    save_dir: &Path,
) -> Result<TorrentMetadata, TorrentError> {
    parse_metadata(bytes, save_dir, true)
}

fn parse_metadata(
    bytes: &[u8],
    save_dir: &Path,
    from_v1_wire: bool,
) -> Result<TorrentMetadata, TorrentError> {
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(invalid(format!(
            "torrent manifest exceeds maximum size of {MAX_TORRENT_BYTES} bytes"
        )));
    }
    let (root, info_raw) = bencode::decode_with_info(bytes)?;
    let root = dict(&root)?;
    let info_raw = info_raw.ok_or_else(|| invalid("missing info dictionary"))?;
    let info = dict(required(root, b"info")?)?;
    let v2 = match info.get(b"meta version".as_slice()) {
        None => false,
        Some(Value::Int(2)) => true,
        Some(Value::Int(version)) => {
            return Err(TorrentError::Unsupported(format!(
                "metadata version {version}"
            )))
        }
        Some(_) => return Err(invalid("meta version must be an integer")),
    };
    let v1 = !v2
        || [b"pieces".as_slice(), b"length", b"files"]
            .iter()
            .any(|key| info.contains_key(*key));
    let version = match (v1, v2) {
        (true, true) => TorrentVersion::Hybrid,
        (true, false) => TorrentVersion::V1,
        (false, true) => TorrentVersion::V2,
        (false, false) => unreachable!(),
    };
    let piece_length = unsigned(required(info, b"piece length")?)?;
    if piece_length == 0
        || (v2 && (piece_length < V2_BLOCK_SIZE || !piece_length.is_power_of_two()))
    {
        return Err(invalid("invalid piece length"));
    }

    let name = info
        .get(b"name".as_slice())
        .map(path_component)
        .transpose()?;
    for component in [
        name,
        info.get(b"name.utf-8".as_slice())
            .map(path_component)
            .transpose()?,
    ]
    .into_iter()
    .flatten()
    {
        safe_join_under(save_dir, component)?;
    }
    let root_padding = attributes(info)?;

    let mut file_specs = if v1 {
        v1_files(info, name, root_padding)?
    } else {
        Vec::new()
    };
    let mut piece_layers = BTreeMap::new();
    if v2 {
        let mut tree_files = Vec::new();
        walk_file_tree(
            dict(required(info, b"file tree")?)?,
            &mut Vec::new(),
            &mut tree_files,
        )?;
        validate_layout(&tree_files, save_dir)?;
        if !from_v1_wire || !v1 || root.contains_key(b"piece layers".as_slice()) {
            piece_layers = parse_piece_layers(root, &tree_files, piece_length)?;
        }
        if v1 {
            match_hybrid_layout(&mut file_specs, &tree_files, piece_length)?;
        } else {
            file_specs = tree_files;
        }
    }

    let resolved_paths = validate_layout(&file_specs, save_dir)?;
    let total_bytes = file_specs.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.size_bytes)
            .ok_or_else(|| invalid("file lengths overflow"))
    })?;
    let piece_count = if v1 {
        total_bytes.div_ceil(piece_length)
    } else {
        file_specs
            .iter()
            .map(|file| file.size_bytes.div_ceil(piece_length))
            .sum()
    };
    let total_pieces =
        usize::try_from(piece_count).map_err(|_| invalid("piece count overflows"))?;
    let pieces = if v1 {
        let hashes = hashes::<20>(required(info, b"pieces")?)?;
        if hashes.len() != total_pieces {
            return Err(invalid("v1 piece count does not match file lengths"));
        }
        hashes
    } else {
        Vec::new()
    };
    let files = file_specs
        .iter()
        .enumerate()
        .map(|(index, spec)| TorrentFileEntry {
            index,
            relative_path: spec.relative_path.clone(),
            size_bytes: spec.size_bytes,
            downloaded_bytes: 0,
            selected: !spec.is_padding,
        })
        .collect();
    let announce_list = root
        .get(b"announce-list".as_slice())
        .and_then(|tiers| list(tiers).ok())
        .into_iter()
        .flatten()
        .filter_map(|tier| list(tier).ok())
        .map(|tier| {
            tier.iter()
                .filter_map(tracker_text)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|tier| !tier.is_empty())
        .collect();
    let private = match info.get(b"private".as_slice()) {
        None | Some(Value::Int(0)) => false,
        Some(Value::Int(1)) => true,
        _ => return Err(invalid("private must be 0 or 1")),
    };

    Ok(TorrentMetadata {
        version,
        info_hash_v1: v1.then(|| Sha1::digest(info_raw).into()),
        info_hash_v2: v2.then(|| Sha256::digest(info_raw).into()),
        name: name.map(str::to_owned),
        piece_length,
        total_bytes,
        total_pieces,
        private,
        announce: root
            .get(b"announce".as_slice())
            .and_then(tracker_text)
            .map(str::to_owned),
        announce_list,
        comment: optional_text(root, b"comment")?,
        created_by: optional_text(root, b"created by")?,
        pieces,
        piece_layers,
        file_specs,
        files,
        resolved_paths,
    })
}

fn tracker_text(value: &Value) -> Option<&str> {
    string(value)
        .ok()
        .filter(|url| allowed_tracker_url(url, false))
}

fn v1_files(
    info: &Dict,
    name: Option<&str>,
    root_padding: bool,
) -> Result<Vec<TorrentFileSpec>, TorrentError> {
    match (
        info.get(b"length".as_slice()),
        info.get(b"files".as_slice()),
    ) {
        (Some(length), None) => Ok(vec![TorrentFileSpec {
            relative_path: name.unwrap_or("torrent-content").to_owned(),
            size_bytes: unsigned(length)?,
            is_padding: root_padding,
            pieces_root: None,
        }]),
        (None, Some(files)) => list(files)?
            .iter()
            .map(|file| {
                let file = dict(file)?;
                let relative_path = v1_path(required(file, b"path")?)?;
                if let Some(utf8) = file.get(b"path.utf-8".as_slice()) {
                    v1_path(utf8)?;
                }
                Ok(TorrentFileSpec {
                    relative_path,
                    size_bytes: unsigned(required(file, b"length")?)?,
                    is_padding: attributes(file)?,
                    pieces_root: None,
                })
            })
            .collect(),
        _ => Err(invalid("v1 torrents require either length or files")),
    }
}

fn v1_path(value: &Value) -> Result<String, TorrentError> {
    let parts = list(value)?;
    if parts.is_empty() {
        return Err(PathSecurityError::Empty.into());
    }
    parts
        .iter()
        .map(path_component)
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}

fn walk_file_tree<'a>(
    tree: &'a Dict,
    path: &mut Vec<&'a str>,
    files: &mut Vec<TorrentFileSpec>,
) -> Result<(), TorrentError> {
    if let Some(properties) = tree.get(b"".as_slice()) {
        if path.is_empty() || tree.len() != 1 {
            return Err(invalid(
                "a v2 file cannot be the tree root or have children",
            ));
        }
        let properties = dict(properties)?;
        let is_padding = attributes(properties)?;
        if is_padding {
            return Err(invalid("v2 file trees cannot contain explicit padding"));
        }
        let size_bytes = unsigned(required(properties, b"length")?)?;
        let pieces_root = properties
            .get(b"pieces root".as_slice())
            .map(fixed_hash::<32>)
            .transpose()?;
        if (size_bytes != 0) != pieces_root.is_some() {
            return Err(invalid(
                "exactly the nonempty v2 files require a pieces root",
            ));
        }
        files.push(TorrentFileSpec {
            relative_path: path.join("/"),
            size_bytes,
            is_padding,
            pieces_root,
        });
        return Ok(());
    }
    if tree.is_empty() {
        return Err(invalid("empty v2 directory"));
    }
    for (name, children) in tree {
        path.push(validate_component(name)?);
        walk_file_tree(dict(children)?, path, files)?;
        path.pop();
    }
    Ok(())
}

fn parse_piece_layers(
    root: &Dict,
    files: &[TorrentFileSpec],
    piece_length: u64,
) -> Result<BTreeMap<[u8; 32], Vec<[u8; 32]>>, TorrentError> {
    let layers = dict(required(root, b"piece layers")?)?
        .iter()
        .map(|(root, hashes_value)| {
            let root = root
                .as_slice()
                .try_into()
                .map_err(|_| invalid("invalid piece layer root length"))?;
            Ok((root, hashes::<32>(hashes_value)?))
        })
        .collect::<Result<BTreeMap<_, _>, TorrentError>>()?;
    let mut expected = BTreeMap::new();
    for file in files.iter().filter(|file| file.size_bytes > piece_length) {
        let root = file.pieces_root.expect("nonempty v2 file has a root");
        let count = file.size_bytes.div_ceil(piece_length);
        if expected
            .insert(root, count)
            .is_some_and(|previous| previous != count)
        {
            return Err(invalid("conflicting sizes for a shared piece layer"));
        }
    }
    if expected.len() != layers.len() {
        return Err(invalid("piece layers do not match the file tree"));
    }
    let mut pad = [0; 32];
    for _ in 0..(piece_length / V2_BLOCK_SIZE).trailing_zeros() {
        pad = merkle_parent(&pad, &pad);
    }
    for (root, count) in expected {
        let hashes = layers
            .get(&root)
            .ok_or_else(|| invalid("missing piece layer"))?;
        if hashes.len() as u64 != count || merkle_root(hashes, pad) != root {
            return Err(invalid(
                "piece layer does not match its root or piece count",
            ));
        }
    }
    Ok(layers)
}

fn merkle_root(hashes: &[[u8; 32]], mut pad: [u8; 32]) -> [u8; 32] {
    let mut level = hashes.to_vec();
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| merkle_parent(&pair[0], pair.get(1).unwrap_or(&pad)))
            .collect();
        pad = merkle_parent(&pad, &pad);
    }
    level[0]
}

fn merkle_parent(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(left);
    hash.update(right);
    hash.finalize().into()
}

fn match_hybrid_layout(
    v1: &mut [TorrentFileSpec],
    v2: &[TorrentFileSpec],
    piece_length: u64,
) -> Result<(), TorrentError> {
    let mut files = v2.iter();
    let mut offset = 0u64;
    for file in v1 {
        if file.is_padding {
            let remainder = offset % piece_length;
            if remainder == 0 || file.size_bytes != piece_length - remainder {
                return Err(invalid("invalid hybrid padding"));
            }
        } else {
            let expected = files
                .next()
                .ok_or_else(|| invalid("hybrid file lists differ"))?;
            if expected.relative_path != file.relative_path
                || expected.size_bytes != file.size_bytes
                || (file.size_bytes != 0 && !offset.is_multiple_of(piece_length))
            {
                return Err(invalid(
                    "hybrid file paths, sizes or piece alignment differ",
                ));
            }
            file.pieces_root = expected.pieces_root;
        }
        offset = offset
            .checked_add(file.size_bytes)
            .ok_or_else(|| invalid("file lengths overflow"))?;
    }
    if files.next().is_some() {
        return Err(invalid("hybrid file lists differ"));
    }
    Ok(())
}

fn validate_layout(
    files: &[TorrentFileSpec],
    save_dir: &Path,
) -> Result<Vec<PathBuf>, TorrentError> {
    if files.is_empty() {
        return Err(invalid("torrent has no files"));
    }
    let mut paths = BTreeSet::new();
    for file in files.iter().filter(|file| !file.is_padding) {
        let key = file.relative_path.to_uppercase();
        if !paths.insert(key) {
            return Err(invalid("duplicate paths in torrent"));
        }
    }
    if paths.is_empty() {
        return Err(invalid("torrent has no non-padding files"));
    }
    for key in &paths {
        for (index, _) in key.match_indices('/') {
            if paths.contains(&key[..index]) {
                return Err(invalid("a torrent path names both a file and a directory"));
            }
        }
    }
    files
        .iter()
        .map(|file| safe_join_under(save_dir, &file.relative_path).map_err(Into::into))
        .collect()
}

fn path_component(value: &Value) -> Result<&str, TorrentError> {
    validate_component(byte_string(value)?)
}

fn validate_component(bytes: &[u8]) -> Result<&str, TorrentError> {
    let name = std::str::from_utf8(bytes).map_err(|_| PathSecurityError::InvalidCharacter)?;
    validate_safe_filename_component(name)?;
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    // Windows treats these additional spellings as DOS devices.
    if matches!(stem.as_str(), "CONIN$" | "CONOUT$")
        || ["COM", "LPT"]
            .iter()
            .any(|prefix| matches!(stem.strip_prefix(prefix), Some("¹" | "²" | "³")))
    {
        return Err(PathSecurityError::ReservedDeviceName.into());
    }
    Ok(name)
}

fn attributes(value: &Dict) -> Result<bool, TorrentError> {
    let attr = value
        .get(b"attr".as_slice())
        .map(byte_string)
        .transpose()?
        .unwrap_or_default();
    if attr.contains(&b'l') || value.contains_key(b"symlink path".as_slice()) {
        return Err(TorrentError::Unsupported(
            "symlink entries are forbidden".into(),
        ));
    }
    Ok(attr.contains(&b'p'))
}

fn required<'a>(dict: &'a Dict, key: &[u8]) -> Result<&'a Value, TorrentError> {
    dict.get(key)
        .ok_or_else(|| invalid(format!("missing {}", String::from_utf8_lossy(key))))
}

fn invalid(message: impl Into<String>) -> TorrentError {
    TorrentError::Metadata(message.into())
}

fn dict(value: &Value) -> Result<&Dict, TorrentError> {
    match value {
        Value::Dict(dict) => Ok(dict),
        _ => Err(invalid("expected a dictionary")),
    }
}

fn list(value: &Value) -> Result<&[Value], TorrentError> {
    match value {
        Value::List(list) => Ok(list),
        _ => Err(invalid("expected a list")),
    }
}

fn byte_string(value: &Value) -> Result<&[u8], TorrentError> {
    match value {
        Value::Bytes(bytes) => Ok(bytes),
        _ => Err(invalid("expected a byte string")),
    }
}

fn string(value: &Value) -> Result<&str, TorrentError> {
    std::str::from_utf8(byte_string(value)?).map_err(|_| invalid("expected UTF-8 text"))
}

fn optional_text(dict: &Dict, key: &[u8]) -> Result<Option<String>, TorrentError> {
    dict.get(key)
        .map(|value| string(value).map(str::to_owned))
        .transpose()
}

fn unsigned(value: &Value) -> Result<u64, TorrentError> {
    match value {
        Value::Int(n) if *n >= 0 => Ok(*n as u64),
        _ => Err(invalid("expected a nonnegative integer")),
    }
}

fn fixed_hash<const N: usize>(value: &Value) -> Result<[u8; N], TorrentError> {
    byte_string(value)?
        .try_into()
        .map_err(|_| invalid("invalid hash length"))
}

fn hashes<const N: usize>(value: &Value) -> Result<Vec<[u8; N]>, TorrentError> {
    let (hashes, remainder) = byte_string(value)?.as_chunks::<N>();
    if !remainder.is_empty() {
        return Err(invalid("invalid concatenated hash length"));
    }
    Ok(hashes.to_vec())
}

/// Build a deterministic v1 manifest whose file is relative to the selected save directory.
pub fn build_v1_single_file_torrent(
    name: &str,
    data: &[u8],
    piece_length: u32,
) -> Result<Vec<u8>, TorrentError> {
    build_v1(name, &[(name, data)], piece_length, true)
}

/// Build a deterministic v1 manifest in the supplied file order. Separate path components with `/`.
pub fn build_v1_multi_file_torrent<P: AsRef<str>, B: AsRef<[u8]>>(
    name: &str,
    files: &[(P, B)],
    piece_length: u32,
) -> Result<Vec<u8>, TorrentError> {
    build_v1(name, files, piece_length, false)
}

fn build_v1<P: AsRef<str>, B: AsRef<[u8]>>(
    name: &str,
    files: &[(P, B)],
    piece_length: u32,
    single_file: bool,
) -> Result<Vec<u8>, TorrentError> {
    if piece_length == 0 {
        return Err(invalid("piece length must be positive"));
    }
    let mut piece_hasher = Sha1::new();
    let mut pieces = Vec::new();
    let mut piece_used = 0;
    let piece_size = piece_length as usize;
    let mut file_values = Vec::new();
    for (path, data) in files {
        let data = data.as_ref();
        let size = i64::try_from(data.len()).map_err(|_| invalid("file length overflows"))?;
        let parts = path
            .as_ref()
            .split('/')
            .map(|part| Value::Bytes(part.as_bytes().to_vec()))
            .collect();
        file_values.push(Value::Dict(BTreeMap::from([
            (b"length".to_vec(), Value::Int(size)),
            (b"path".to_vec(), Value::List(parts)),
        ])));
        let mut remaining = data;
        while !remaining.is_empty() {
            let (take, rest) = remaining.split_at(remaining.len().min(piece_size - piece_used));
            remaining = rest;
            piece_hasher.update(take);
            piece_used += take.len();
            if piece_used == piece_size {
                pieces.extend_from_slice(&piece_hasher.finalize_reset());
                piece_used = 0;
            }
        }
    }
    if piece_used != 0 {
        pieces.extend_from_slice(&piece_hasher.finalize());
    }
    let mut info = BTreeMap::from([
        (b"name".to_vec(), Value::Bytes(name.as_bytes().to_vec())),
        (b"piece length".to_vec(), Value::Int(piece_length.into())),
        (b"pieces".to_vec(), Value::Bytes(pieces)),
    ]);
    if single_file {
        info.insert(
            b"length".to_vec(),
            dict(&file_values[0])?[b"length".as_slice()].clone(),
        );
    } else {
        info.insert(b"files".to_vec(), Value::List(file_values));
    }
    let bytes = bencode::encode(&Value::Dict(BTreeMap::from([(
        b"info".to_vec(),
        Value::Dict(info),
    )])));
    parse_torrent_bytes(&bytes, Path::new(""))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::magnet::tests::{BLOCKED_TRACKERS, PUBLIC_TRACKERS};

    #[test]
    fn filters_announce_and_announce_list_without_rejecting_the_torrent() {
        let original = build_v1_single_file_torrent("example.bin", b"data", 4).unwrap();
        let (root, _) = bencode::decode_with_info(&original).unwrap();
        let Value::Dict(mut root) = root else {
            unreachable!()
        };
        let tier = |urls: &[&str]| {
            Value::List(
                urls.iter()
                    .map(|url| Value::Bytes(url.as_bytes().to_vec()))
                    .collect(),
            )
        };
        let mut mixed = PUBLIC_TRACKERS.to_vec();
        mixed.extend_from_slice(BLOCKED_TRACKERS);
        root.insert(
            b"announce-list".to_vec(),
            Value::List(vec![tier(BLOCKED_TRACKERS), tier(&mixed)]),
        );
        for url in PUBLIC_TRACKERS.iter().chain(BLOCKED_TRACKERS) {
            root.insert(b"announce".to_vec(), Value::Bytes(url.as_bytes().to_vec()));
            let parsed =
                parse_torrent_bytes(&bencode::encode(&Value::Dict(root.clone())), Path::new(""))
                    .unwrap();
            assert_eq!(
                parsed.announce.as_deref(),
                PUBLIC_TRACKERS.contains(url).then_some(*url)
            );
            assert_eq!(parsed.announce_list, [PUBLIC_TRACKERS]);
        }
    }
}
