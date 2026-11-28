use crate::{
    paths::{open_unlinked_local_file, reject_existing_links},
    rss,
};
use quick_xml::{events::Event, name::ResolveResult, NsReader};
use slipstream_protocol::{sanitize_filename, MetalinkFileEntry};
use std::{io::Read, path::Path};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_FILES: usize = 500;
const V4: &[u8] = b"urn:ietf:params:xml:ns:metalink";
const V3: &[u8] = b"http://www.metalinker.org/";

pub fn parse_metalink_file(
    path: &Path,
    allow_loopback: bool,
) -> Result<Vec<MetalinkFileEntry>, String> {
    let path = reject_existing_links(path).map_err(|error| error.to_string())?;
    let mut file = open_unlinked_local_file(&path)
        .map_err(|error| error.to_string())?
        .take(MAX_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    parse_metalink_bytes(&bytes, allow_loopback)
}

enum FieldKind {
    Size,
    Hash,
    Url,
}

struct Field {
    kind: FieldKind,
    depth: usize,
    value: String,
}

pub fn parse_metalink_bytes(
    bytes: &[u8],
    allow_loopback: bool,
) -> Result<Vec<MetalinkFileEntry>, String> {
    if bytes.len() > MAX_BYTES {
        return Err("Metalink exceeds 2 MiB".into());
    }
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut root_namespace: Option<Vec<u8>> = None;
    let mut current = None::<MetalinkFileEntry>;
    let mut current_depth = 0;
    let mut field = None::<Field>;
    let mut count = 0;
    let mut files = Vec::new();

    loop {
        let decoder = reader.decoder();
        let (namespace, event) = reader
            .read_resolved_event()
            .map_err(|error| error.to_string())?;
        match event {
            Event::Start(tag) => {
                let namespace = match namespace {
                    ResolveResult::Bound(namespace) => namespace.0,
                    ResolveResult::Unbound => &[],
                    ResolveResult::Unknown(_) => return Err("unbound Metalink namespace".into()),
                };
                let depth = stack.len() + 1;
                let name = tag.local_name();
                let name = name.as_ref();
                if depth > MAX_DEPTH {
                    return Err("Metalink nesting exceeds 64 levels".into());
                }
                if depth == 1 {
                    if root_namespace.is_some()
                        || name != b"metalink"
                        || !matches!(namespace, V4 | V3 | b"http://www.metalinker.org" | b"")
                    {
                        return Err("unexpected Metalink root or namespace".into());
                    }
                    root_namespace = Some(namespace.to_vec());
                }
                let in_namespace = root_namespace.as_deref() == Some(namespace);
                let v4 = root_namespace.as_deref() == Some(V4);
                // A child from another namespace cannot participate in a Metalink path.
                stack.push(if in_namespace {
                    name.to_vec()
                } else {
                    Vec::new()
                });
                let parent = depth.checked_sub(2).map(|index| stack[index].as_slice());
                let attr = |key| -> Result<Option<String>, String> {
                    tag.try_get_attribute(key)
                        .map_err(|error| error.to_string())?
                        .map(|attr| {
                            attr.decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                decoder,
                            )
                            .map(|value| value.into_owned())
                            .map_err(|error| error.to_string())
                        })
                        .transpose()
                };
                if !in_namespace {
                    continue;
                }
                let file_node = name == b"file"
                    && ((v4 && depth == 2) || (!v4 && depth == 3 && parent == Some(b"files")));
                if file_node {
                    count += 1;
                    if count > MAX_FILES {
                        return Err("Metalink exceeds 500 files".into());
                    }
                    current_depth = depth;
                    current = attr(b"name")?
                        .and_then(|raw| sanitize_filename(raw.rsplit(['/', '\\']).next()?).ok())
                        .map(|filename| MetalinkFileEntry {
                            filename,
                            size_bytes: None,
                            sha256: None,
                            mirrors: Vec::new(),
                        });
                } else if current.is_some() {
                    let direct = depth == current_depth + 1;
                    let grouped = !v4 && depth == current_depth + 2;
                    let kind = match name {
                        b"size" if direct => Some(FieldKind::Size),
                        b"url" if (v4 && direct) || (grouped && parent == Some(b"resources")) => {
                            Some(FieldKind::Url)
                        }
                        b"hash"
                            if (v4 && direct) || (grouped && parent == Some(b"verification")) =>
                        {
                            attr(b"type")?
                                .filter(|kind| {
                                    kind.eq_ignore_ascii_case("sha-256")
                                        || kind.eq_ignore_ascii_case("sha256")
                                })
                                .map(|_| FieldKind::Hash)
                        }
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        field = Some(Field {
                            kind,
                            depth,
                            value: String::new(),
                        });
                    }
                }
            }
            Event::End(_) => {
                if field
                    .as_ref()
                    .is_some_and(|field| field.depth == stack.len())
                {
                    let Field { kind, value, .. } = field.take().unwrap();
                    let entry = current.as_mut().unwrap();
                    let value = value.trim();
                    match kind {
                        FieldKind::Size => entry.size_bytes = value.parse().ok(),
                        FieldKind::Hash
                            if value.len() == 64
                                && value.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
                        {
                            entry.sha256 = Some(value.to_ascii_lowercase());
                        }
                        FieldKind::Url => {
                            if let Ok(url) = rss::checked_url(value, allow_loopback) {
                                let url = url.to_string();
                                if !entry.mirrors.contains(&url) {
                                    entry.mirrors.push(url);
                                }
                            }
                        }
                        FieldKind::Hash => {}
                    }
                }
                if current_depth != 0 && stack.len() == current_depth {
                    files.extend(current.take());
                    current_depth = 0;
                }
                stack.pop();
            }
            Event::Text(ref text)
                if field
                    .as_ref()
                    .is_some_and(|field| field.depth == stack.len()) =>
            {
                field
                    .as_mut()
                    .unwrap()
                    .value
                    .push_str(&text.decode().map_err(|error| error.to_string())?);
            }
            Event::CData(ref text)
                if field
                    .as_ref()
                    .is_some_and(|field| field.depth == stack.len()) =>
            {
                field
                    .as_mut()
                    .unwrap()
                    .value
                    .push_str(&text.decode().map_err(|error| error.to_string())?);
            }
            Event::GeneralRef(reference)
                if field
                    .as_ref()
                    .is_some_and(|field| field.depth == stack.len()) =>
            {
                let reference = reference.decode().map_err(|error| error.to_string())?;
                let ch = rss::decode_reference(&reference).ok_or("invalid Metalink entity")?;
                field.as_mut().unwrap().value.push(ch);
            }
            Event::DocType(_) => return Err("Metalink must not contain a document type".into()),
            Event::Eof if root_namespace.is_some() && stack.is_empty() => return Ok(files),
            Event::Eof => return Err("incomplete Metalink XML".into()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v4_namespace_and_sanitizes_traversal() {
        let hash = "AB".repeat(32);
        let xml = format!(
            r#"<m:metalink xmlns:m="urn:ietf:params:xml:ns:metalink"><m:file name="../archive.zip">
          <m:size>42</m:size><m:hash type="sha-256">{hash}</m:hash><m:url>https://example.org/a?p=1&amp;q=2</m:url>
          <m:url>http://127.0.0.1/archive.zip</m:url><m:url>file:///archive.zip</m:url>
          </m:file></m:metalink>"#
        );
        let files = parse_metalink_bytes(xml.as_bytes(), false).unwrap();
        assert_eq!(
            files,
            vec![MetalinkFileEntry {
                filename: "archive.zip".into(),
                size_bytes: Some(42),
                sha256: Some(hash.to_ascii_lowercase()),
                mirrors: vec!["https://example.org/a?p=1&q=2".into()],
            }]
        );
    }

    #[test]
    fn parses_v3_resources_and_verification() {
        let hash = "cd".repeat(32);
        let xml = format!(
            r#"<metalink xmlns="http://www.metalinker.org/"><files><file name="file.zip">
          <verification><hash type="sha256">{hash}</hash></verification>
          <resources><url type="http">http://127.0.0.1:8080/file.zip</url></resources>
          </file></files></metalink>"#
        );
        let files = parse_metalink_bytes(xml.as_bytes(), true).unwrap();
        assert_eq!(files[0].sha256, Some(hash));
        assert_eq!(files[0].mirrors.len(), 1);
    }

    #[test]
    fn rejects_doctype_and_enforces_limits() {
        assert!(parse_metalink_bytes(b"<!DOCTYPE metalink><metalink/>", false).is_err());
        let nested = format!(
            "<metalink>{}{}</metalink>",
            "<x>".repeat(64),
            "</x>".repeat(64)
        );
        assert!(parse_metalink_bytes(nested.as_bytes(), false).is_err());
        let many = format!(
            "<metalink><files>{}</files></metalink>",
            r#"<file name="item"/>"#.repeat(501)
        );
        assert!(parse_metalink_bytes(many.as_bytes(), false).is_err());
        assert!(parse_metalink_bytes(&vec![b'x'; MAX_BYTES + 1], false).is_err());
    }
}
