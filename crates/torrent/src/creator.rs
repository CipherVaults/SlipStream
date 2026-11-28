use crate::{
    bencode::{self, BencodeValue as Value},
    magnet::validated_tracker_urls,
    parse_torrent_bytes,
    storage::{is_link, open_regular_file, path_text, reject_existing_links, validate_local_path},
    TorrentError, MAX_TORRENT_BYTES,
};
use chrono::Utc;
use sha1::{Digest, Sha1};
use slipstream_protocol::{
    validate_safe_filename_component, CreateTorrentRequest, CreateTorrentResult, PathSecurityError,
};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MAX_FILES: usize = 10_000;
const HASH_BYTES: usize = 20;

struct SourceFile {
    path: PathBuf,
    relative_path: String,
    len: i64,
}

/// Hash a file or directory and atomically publish its BitTorrent v1 manifest.
pub fn create_v1_torrent(req: &CreateTorrentRequest) -> Result<CreateTorrentResult, TorrentError> {
    let source = absolute_local_path(&req.source_path)?;
    let output_path = absolute_local_path(&req.output_path)?;
    let name = safe_name(&source)?;
    let source_parent = source.parent().ok_or(PathSecurityError::Empty)?;
    validate_output_path(&output_path)?;
    let trackers = validated_tracker_urls(&req.trackers, false)?;
    let source_metadata = fs::symlink_metadata(&source)?;
    if is_link(&source_metadata) {
        return Err(PathSecurityError::EscapesBaseDir.into());
    }
    let single_file = source_metadata.is_file();
    let mut files = if single_file {
        vec![source_file(source.clone(), name.into(), &source_metadata)?]
    } else if source_metadata.is_dir() {
        collect_files(&source)?
    } else {
        return Err(TorrentError::InvalidState(
            "torrent source must be a regular file or directory".into(),
        ));
    };
    if output_path.try_exists()? {
        let canonical_output = fs::canonicalize(&output_path)?;
        let canonical_source = fs::canonicalize(&source)?;
        if canonical_output == canonical_source
            || (!single_file && canonical_output.starts_with(&canonical_source))
        {
            return Err(TorrentError::InvalidState(
                "torrent output would replace a source file".into(),
            ));
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let total_bytes = files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.len as u64)
            .ok_or_else(|| TorrentError::Metadata("torrent file lengths overflow".into()))
    })?;
    if total_bytes == 0 {
        return Err(TorrentError::InvalidState("torrent source is empty".into()));
    }
    let piece_length = piece_length(req.piece_length_kib, total_bytes)?;
    let piece_count = total_bytes.div_ceil(piece_length as u64);
    if piece_count > (MAX_TORRENT_BYTES / HASH_BYTES) as u64 {
        return Err(manifest_too_large());
    }
    let pieces = hash_files(&files, piece_length, piece_count as usize)?;
    let mut info = BTreeMap::from([
        (b"name".to_vec(), string(name)),
        (b"piece length".to_vec(), Value::Int(piece_length as i64)),
        (b"pieces".to_vec(), Value::Bytes(pieces)),
    ]);
    if req.private {
        info.insert(b"private".to_vec(), Value::Int(1));
    }
    if single_file {
        info.insert(b"length".to_vec(), Value::Int(files[0].len));
    } else {
        info.insert(
            b"files".to_vec(),
            Value::List(
                files
                    .iter()
                    .map(|file| {
                        Value::Dict(BTreeMap::from([
                            (b"length".to_vec(), Value::Int(file.len)),
                            (
                                b"path".to_vec(),
                                Value::List(file.relative_path.split('/').map(string).collect()),
                            ),
                        ]))
                    })
                    .collect(),
            ),
        );
    }
    let mut manifest = BTreeMap::from([
        (b"info".to_vec(), Value::Dict(info)),
        (b"created by".to_vec(), string("SlipStream")),
        (
            b"creation date".to_vec(),
            Value::Int(Utc::now().timestamp()),
        ),
    ]);
    if let Some(first) = trackers.first() {
        manifest.insert(b"announce".to_vec(), string(first));
        manifest.insert(
            b"announce-list".to_vec(),
            Value::List(
                trackers
                    .iter()
                    .map(|tracker| Value::List(vec![string(tracker)]))
                    .collect(),
            ),
        );
    }
    if let Some(comment) = &req.comment {
        manifest.insert(b"comment".to_vec(), string(comment));
    }
    let encoded = bencode::encode(&Value::Dict(manifest));
    if encoded.len() > MAX_TORRENT_BYTES {
        return Err(manifest_too_large());
    }
    let metadata = parse_torrent_bytes(&encoded, source_parent)?;
    let info_hash = metadata
        .info_hash_v1
        .map(hex::encode)
        .ok_or_else(|| TorrentError::Metadata("created torrent has no v1 info hash".into()))?;
    write_manifest(&output_path, &encoded)?;
    Ok(CreateTorrentResult {
        output_path: path_text(&output_path)?.to_owned(),
        info_hash,
        total_bytes,
        piece_count: metadata.total_pieces,
        file_count: files.len(),
    })
}

fn absolute_local_path(raw: &str) -> Result<PathBuf, TorrentError> {
    let path = Path::new(raw);
    validate_local_path(path)?;
    let path: PathBuf = std::path::absolute(path)?.components().collect();
    validate_local_path(&path)?;
    reject_existing_links(&path)?;
    Ok(path)
}

fn safe_name(path: &Path) -> Result<&str, TorrentError> {
    let name = path.file_name().ok_or(PathSecurityError::Empty)?;
    Ok(validate_safe_filename_component(path_text(Path::new(
        name,
    ))?)?)
}

fn validate_output_path(output: &Path) -> Result<(), TorrentError> {
    safe_name(output)?;
    if !output
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("torrent"))
    {
        return Err(TorrentError::InvalidState(
            "torrent output must have a .torrent extension".into(),
        ));
    }
    let parent = output.parent().ok_or(PathSecurityError::Empty)?;
    reject_existing_links(output)?;
    if !fs::symlink_metadata(parent)?.is_dir() {
        return Err(TorrentError::InvalidState(
            "torrent output parent must be a directory".into(),
        ));
    }
    match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => Ok(()),
        Ok(_) => Err(TorrentError::InvalidState(
            "torrent output must be a regular file".into(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn collect_files(source: &Path) -> Result<Vec<SourceFile>, TorrentError> {
    let mut files = Vec::new();
    let mut dirs = vec![(source.to_owned(), String::new())];
    while let Some((dir, relative_dir)) = dirs.pop() {
        reject_existing_links(&dir)?;
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let name = safe_name(&path)?;
            let relative_path = if relative_dir.is_empty() {
                name.to_owned()
            } else {
                format!("{relative_dir}/{name}")
            };
            let metadata = fs::symlink_metadata(&path)?;
            if is_link(&metadata) {
                return Err(PathSecurityError::EscapesBaseDir.into());
            }
            if metadata.is_dir() {
                dirs.push((path, relative_path));
            } else if metadata.is_file() {
                if files.len() == MAX_FILES {
                    return Err(TorrentError::InvalidState(format!(
                        "torrent source exceeds {MAX_FILES} files"
                    )));
                }
                files.push(source_file(path, relative_path, &metadata)?);
            } else {
                return Err(TorrentError::InvalidState(
                    "torrent directory contains a non-regular file".into(),
                ));
            }
        }
    }
    Ok(files)
}

fn source_file(
    path: PathBuf,
    relative_path: String,
    metadata: &fs::Metadata,
) -> Result<SourceFile, TorrentError> {
    let len = i64::try_from(metadata.len())
        .map_err(|_| TorrentError::Metadata("torrent file length overflows".into()))?;
    Ok(SourceFile {
        path,
        relative_path,
        len,
    })
}

fn piece_length(kib: u32, total_bytes: u64) -> Result<usize, TorrentError> {
    if kib == 0 {
        let target = (total_bytes / 1200).clamp(256 * 1024, 16 * 1024 * 1024);
        return Ok(target.next_power_of_two() as usize);
    }
    if !(16..=65536).contains(&kib) || !kib.is_power_of_two() {
        return Err(TorrentError::InvalidState(
            "piece length must be a power of two between 16 and 65536 KiB".into(),
        ));
    }
    Ok(kib as usize * 1024)
}

fn hash_files(
    files: &[SourceFile],
    piece_length: usize,
    piece_count: usize,
) -> Result<Vec<u8>, TorrentError> {
    let mut pieces = Vec::with_capacity(piece_count * HASH_BYTES);
    let mut buffer = vec![0; piece_length.min(1024 * 1024)];
    let mut sha1 = Sha1::new();
    let mut piece_used = 0;
    for source in files {
        let mut file = open_regular_file(&source.path)?;
        let mut remaining = source.len as u64;
        while remaining > 0 {
            let len = (buffer.len().min(piece_length - piece_used) as u64).min(remaining) as usize;
            file.read_exact(&mut buffer[..len])?;
            sha1.update(&buffer[..len]);
            remaining -= len as u64;
            piece_used += len;
            if piece_used == piece_length {
                pieces.extend_from_slice(&sha1.finalize_reset());
                piece_used = 0;
            }
        }
        if file.read(&mut buffer[..1])? != 0 {
            return Err(TorrentError::InvalidState(
                "source file grew while hashing torrent pieces".into(),
            ));
        }
    }
    if piece_used != 0 {
        pieces.extend_from_slice(&sha1.finalize());
    }
    Ok(pieces)
}

fn string(value: &str) -> Value {
    Value::Bytes(value.as_bytes().to_vec())
}

fn manifest_too_large() -> TorrentError {
    TorrentError::Metadata(format!(
        "torrent manifest exceeds maximum size of {MAX_TORRENT_BYTES} bytes"
    ))
}

fn write_manifest(output: &Path, encoded: &[u8]) -> Result<(), TorrentError> {
    validate_output_path(output)?;
    let temp = output.with_file_name(format!(".slipstream-create-{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let write = file.write_all(encoded).and_then(|()| file.sync_all());
    drop(file);
    let result = (|| {
        write?;
        validate_output_path(output)?;
        fs::rename(&temp, output)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TorrentVersion;
    use tempfile::tempdir;

    fn request(source: &Path, output: &Path) -> CreateTorrentRequest {
        CreateTorrentRequest {
            source_path: source.to_str().unwrap().to_owned(),
            output_path: output.to_str().unwrap().to_owned(),
            trackers: Vec::new(),
            piece_length_kib: 0,
            private: false,
            comment: None,
        }
    }

    #[test]
    fn single_file_round_trips_with_manifest_fields() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("sample.bin");
        let output = dir.path().join("sample.torrent");
        let data = b"the complete payload";
        fs::write(&source, data).unwrap();
        let mut req = request(&source, &output);
        req.private = true;
        req.comment = Some("test comment".into());
        req.trackers = vec![
            "  https://tracker.example/announce ".into(),
            String::new(),
            "udp://tracker.example:6969/announce".into(),
            "https://tracker.example/announce".into(),
        ];
        let result = create_v1_torrent(&req).unwrap();
        let encoded = fs::read(output).unwrap();
        let parsed = parse_torrent_bytes(&encoded, dir.path()).unwrap();
        assert_eq!(parsed.version, TorrentVersion::V1);
        assert_eq!(parsed.name.as_deref(), Some("sample.bin"));
        assert_eq!(parsed.piece_length, 256 * 1024);
        assert_eq!(parsed.pieces, [<[u8; 20]>::from(Sha1::digest(data))]);
        assert!(parsed.private);
        assert_eq!(parsed.comment.as_deref(), Some("test comment"));
        assert_eq!(parsed.created_by.as_deref(), Some("SlipStream"));
        assert_eq!(
            parsed.announce.as_deref(),
            Some("https://tracker.example/announce")
        );
        assert_eq!(
            parsed.announce_list,
            [
                ["https://tracker.example/announce"],
                ["udp://tracker.example:6969/announce"],
            ]
        );
        let Value::Dict(root) = bencode::decode(&encoded).unwrap() else {
            panic!("expected a metainfo dictionary");
        };
        assert!(matches!(root[b"creation date".as_slice()], Value::Int(date) if date > 0));
        assert_eq!(Path::new(&result.output_path), Path::new(&req.output_path));
        assert_eq!(result.total_bytes, data.len() as u64);
        assert_eq!(result.file_count, 1);
        assert_eq!(result.piece_count, 1);
        assert_eq!(
            result.info_hash,
            hex::encode(Sha1::digest(
                bencode::extract_info_raw_bytes(&encoded).unwrap()
            ))
        );
    }

    #[test]
    fn directory_hashes_across_files_in_sorted_path_order() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("bundle");
        let output = dir.path().join("bundle.torrent");
        fs::create_dir_all(source.join("nested")).unwrap();
        let first = vec![1; 9000];
        let second = vec![2; 24_000];
        fs::write(source.join("nested/z.bin"), &second).unwrap();
        fs::write(source.join("a.bin"), &first).unwrap();
        fs::write(source.join("empty.bin"), []).unwrap();
        let mut req = request(&source, &output);
        req.piece_length_kib = 16;
        let result = create_v1_torrent(&req).unwrap();
        let parsed = parse_torrent_bytes(&fs::read(output).unwrap(), dir.path()).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("bundle"));
        assert_eq!(
            parsed
                .file_specs
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["a.bin", "empty.bin", "nested/z.bin"]
        );
        let payload = [first, second].concat();
        let hashes: Vec<[u8; 20]> = payload
            .chunks(16 * 1024)
            .map(|piece| Sha1::digest(piece).into())
            .collect();
        assert_eq!(parsed.pieces, hashes);
        assert_eq!(result.piece_count, 3);
        assert_eq!(result.file_count, 3);
        assert_eq!(result.total_bytes, payload.len() as u64);
        assert_eq!(piece_length(0, 1200 * 700_000).unwrap(), 1024 * 1024);
        assert_eq!(piece_length(0, u64::MAX).unwrap(), 16 * 1024 * 1024);
    }

    #[test]
    fn validates_requests_before_atomically_replacing_existing_output() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("input.bin");
        let output = dir.path().join("existing.torrent");
        fs::write(&source, b"real data").unwrap();
        fs::write(&output, b"keep until validated").unwrap();
        let mut req = request(&source, &output);
        req.trackers = vec![
            "https://tracker.example/announce".into(),
            "http://127.0.0.1/announce".into(),
        ];
        assert!(matches!(
            create_v1_torrent(&req),
            Err(TorrentError::InvalidState(_))
        ));
        req.trackers.clear();
        for kib in [1, 15, 24, 65537] {
            req.piece_length_kib = kib;
            assert!(matches!(
                create_v1_torrent(&req),
                Err(TorrentError::InvalidState(_))
            ));
        }
        assert_eq!(fs::read(&output).unwrap(), b"keep until validated");
        req.piece_length_kib = 16;
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let lock = OpenOptions::new()
                .read(true)
                .share_mode(1 | 2)
                .open(&output)
                .unwrap();
            assert!(create_v1_torrent(&req).is_err());
            assert_eq!(fs::read(&output).unwrap(), b"keep until validated");
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
            drop(lock);
        }
        create_v1_torrent(&req).unwrap();
        assert!(parse_torrent_bytes(&fs::read(&output).unwrap(), dir.path()).is_ok());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);

        let output_as_dir = dir.path().join("directory.torrent");
        fs::create_dir(&output_as_dir).unwrap();
        assert!(create_v1_torrent(&request(&source, &output_as_dir)).is_err());
        assert!(output_as_dir.is_dir());
        assert!(create_v1_torrent(&request(&output, &output)).is_err());
    }

    #[test]
    fn rejects_unsafe_paths_and_directory_links() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let outside = dir.path().join("outside");
        let output = dir.path().join("result.torrent");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(source.join("good"), b"good").unwrap();
        fs::write(outside.join("secret"), b"elsewhere").unwrap();
        let link = source.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(windows)]
        assert!(std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success());
        for input in [&source, &link, &link.join("secret")] {
            assert!(matches!(
                create_v1_torrent(&request(input, &output)),
                Err(TorrentError::PathSecurity(_))
            ));
        }
        let request = request(&outside, &link.join("out.torrent"));
        assert!(matches!(
            create_v1_torrent(&request),
            Err(TorrentError::PathSecurity(_))
        ));
        let mut req = request;
        req.output_path = output.to_str().unwrap().to_owned();
        for bad in ["", "file\nname", "data:ads", r"\\server\share\file"] {
            req.source_path = bad.into();
            assert!(matches!(
                create_v1_torrent(&req),
                Err(TorrentError::PathSecurity(_))
            ));
        }
        assert!(!output.exists());
        assert!(!outside.join("out.torrent").exists());
        #[cfg(windows)]
        fs::remove_dir(link).unwrap();
    }
}
