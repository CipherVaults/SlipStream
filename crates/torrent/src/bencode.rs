use crate::TorrentError;
use std::{collections::BTreeMap, ops::Range};

pub const MAX_BENCODE_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BencodeValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(BTreeMap<Vec<u8>, BencodeValue>),
}

pub fn decode(data: &[u8]) -> Result<BencodeValue, TorrentError> {
    decode_with_info(data).map(|(value, _)| value)
}

pub fn encode(value: &BencodeValue) -> Vec<u8> {
    fn write_bytes(bytes: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(bytes.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(bytes);
    }

    fn write(value: &BencodeValue, out: &mut Vec<u8>) {
        match value {
            BencodeValue::Int(value) => {
                out.push(b'i');
                out.extend_from_slice(value.to_string().as_bytes());
                out.push(b'e');
            }
            BencodeValue::Bytes(bytes) => write_bytes(bytes, out),
            BencodeValue::List(values) => {
                out.push(b'l');
                for value in values {
                    write(value, out);
                }
                out.push(b'e');
            }
            BencodeValue::Dict(values) => {
                out.push(b'd');
                for (key, value) in values {
                    write_bytes(key, out);
                    write(value, out);
                }
                out.push(b'e');
            }
        }
    }

    let mut out = Vec::new();
    write(value, &mut out);
    out
}

pub fn extract_info_raw_bytes(data: &[u8]) -> Result<&[u8], TorrentError> {
    let (_, info) = decode_with_info(data)?;
    info.ok_or_else(|| TorrentError::Bencode("missing top-level info dictionary".into()))
}

pub(crate) fn decode_with_info(data: &[u8]) -> Result<(BencodeValue, Option<&[u8]>), TorrentError> {
    let mut parser = Parser {
        data,
        pos: 0,
        info: None,
    };
    let value = parser.value(0)?;
    if parser.pos != data.len() {
        return Err(parser.error("trailing data"));
    }
    Ok((value, parser.info.map(|range| &data[range])))
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    info: Option<Range<usize>>,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> TorrentError {
        TorrentError::Bencode(format!("{message} at byte {}", self.pos))
    }

    fn peek(&self) -> Result<u8, TorrentError> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.error("unexpected end"))
    }

    fn until(&mut self, delimiter: u8) -> Result<&'a [u8], TorrentError> {
        let start = self.pos;
        let offset = self.data[start..]
            .iter()
            .position(|byte| *byte == delimiter)
            .ok_or_else(|| self.error("missing delimiter"))?;
        self.pos = start + offset + 1;
        Ok(&self.data[start..start + offset])
    }

    fn bytes(&mut self) -> Result<&'a [u8], TorrentError> {
        let digits = self.until(b':')?;
        if !canonical_digits(digits) {
            return Err(self.error("invalid byte string length"));
        }
        let length: usize = std::str::from_utf8(digits)
            .unwrap()
            .parse()
            .map_err(|_| self.error("byte string length overflow"))?;
        let end = self
            .pos
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| self.error("truncated byte string"))?;
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn value(&mut self, depth: usize) -> Result<BencodeValue, TorrentError> {
        if depth > MAX_BENCODE_DEPTH {
            return Err(self.error("maximum nesting depth exceeded"));
        }
        match self.peek()? {
            b'i' => {
                self.pos += 1;
                let token = self.until(b'e')?;
                let magnitude = token.strip_prefix(b"-").unwrap_or(token);
                if !canonical_digits(magnitude) || token == b"-0" {
                    return Err(self.error("noncanonical integer"));
                }
                std::str::from_utf8(token)
                    .unwrap()
                    .parse()
                    .map(BencodeValue::Int)
                    .map_err(|_| self.error("integer overflow"))
            }
            b'0'..=b'9' => self
                .bytes()
                .map(|bytes| BencodeValue::Bytes(bytes.to_vec())),
            b'l' => {
                self.pos += 1;
                let mut values = Vec::new();
                while self.peek()? != b'e' {
                    values.push(self.value(depth + 1)?);
                }
                self.pos += 1;
                Ok(BencodeValue::List(values))
            }
            b'd' => {
                self.pos += 1;
                let mut values = BTreeMap::new();
                while self.peek()? != b'e' {
                    let key = self.bytes()?;
                    let start = self.pos;
                    let value = self.value(depth + 1)?;
                    if depth == 0 && key == b"info" && matches!(value, BencodeValue::Dict(_)) {
                        self.info = Some(start..self.pos);
                    }
                    if values.insert(key.to_vec(), value).is_some() {
                        return Err(self.error("duplicate dictionary key"));
                    }
                }
                self.pos += 1;
                Ok(BencodeValue::Dict(values))
            }
            _ => Err(self.error("unknown bencode token")),
        }
    }
}

fn canonical_digits(digits: &[u8]) -> bool {
    !digits.is_empty()
        && (digits.len() == 1 || digits[0] != b'0')
        && digits.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_info_order_on_extraction_and_sorts_on_encoding() {
        let bytes = b"d4:infod1:zi0e1:ai-1eee";
        assert_eq!(extract_info_raw_bytes(bytes).unwrap(), b"d1:zi0e1:ai-1ee");
        assert_eq!(encode(&decode(bytes).unwrap()), b"d4:infod1:ai-1e1:zi0eee");
    }

    #[test]
    fn rejects_malformed_and_unbounded_values() {
        for input in [
            "i-0e",
            "i+1e",
            "i01e",
            "i-01e",
            "ie",
            "i--1e",
            "i9223372036854775808e",
            "01:a",
            "2:a",
            "0:extra",
            "d1:ai0e1:ai1ee",
        ] {
            assert!(decode(input.as_bytes()).is_err(), "{input}");
        }
        let too_deep = format!("{}i0e{}", "l".repeat(65), "e".repeat(65));
        assert!(decode(too_deep.as_bytes()).is_err());
        assert_eq!(
            decode(b"i-9223372036854775808e").unwrap(),
            BencodeValue::Int(i64::MIN)
        );
    }
}
